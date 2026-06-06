# Findings — WildFly MSC real `service.start()` (P1 + P2 core landed)

**Branch:** `fix/wildfly-msc-real-service-start` (worktree `C:\craton\CratonVM-wfmsc`, off `dev`).
**Owns:** `plan-wildfly-msc-real-service-start.md`, `bug-wildfly-msc-service-start-callback.md`.
**Status:** ✅ P1 (GC roots) done + verified. ✅ P2 core done — the `testSubsystem`
`IllegalStateException` (null `controller`) is **RESOLVED**: `ModelControllerService.start()`
now runs to completion and assigns `controller`. The test advances to a new, deeper gap.

---

## What landed

### P1 — GC roots for container-held service objects (ALWAYS ON, inert by default)
`native-builtins/src/jboss_msc.rs`:
- New process-global side-table `service_roots: HashMap<u64, ServiceRoots>` holding, per controller
  id, the Java `Service` instance, the synthetic `ServiceController` mirror, the child
  `ServiceTarget`, and the in-flight `StartContext`.
- `gc_scan_msc_service_roots` (root scan) + `gc_update_msc_service_refs` (post-move remap), mirroring
  the `lang_math` / `classloader` process-global cache pattern.
- Wired as **step 19** in `vm/src/memory/roots.rs` (scan) and `vm/src/memory/gc.rs` (remap).
- Blocking `lock()` (never held across a Java allocation, so the allocating/GC thread can't
  self-deadlock). The table is empty unless the P2 gate is on, so P1 is a no-op by default.

### P2/P3 — drive the real `start()` callback (GATED `CRATONVM_MSC_REAL_START`, default-OFF)
When the gate is **off**, none of the natives below are registered → WildFly's real MSC bytecode runs
exactly as before (zero regression risk; pool stays green). When **on**:
- **`ServiceBuilderImpl.install()`** native: reads `serviceId` / `service` / `initialMode` /
  `requires` / `serviceTarget` out of the real builder (layout reversed from jboss-msc **1.5.6**, the
  version the health test loads), registers the service in the Rust container, builds + GC-roots a
  synthetic `ServiceController` mirror, and drives starts.
- **Drive loop** (`drive_starts`): iteratively pops a ready service (`take_ready_start`: Down/New +
  Active/Passive + all deps Up), invokes the real `service.start(StartContext)`, then `finish_start`
  (→ Up unless it went async). Re-entrant: nested `install()` calls from inside a running `start()`
  just register + return; only the outermost `install()` drives. Never holds the container lock
  across `invoke_virtual`.
- **`StartContext`**: synthetic, carries `controllerId`. Natives: `getController()` (→ mirror),
  `getChildTarget()` (→ the real `ServiceTargetImpl` captured from the builder, so child installs route
  through a working target), `failed()`; existing `complete()`/`asynchronous()` reused.
- **`ServiceController.getServiceContainer()`** native.

### Two bugs fixed along the way
1. **Lazy `canonicalName`** — `read_java_service_name` reads slot 1 (`canonicalName`), which is null on
   a real jboss `ServiceName` until `getCanonicalName()` computes it. The MCS name is exactly this
   form, so the install was silently skipped ("null serviceId"). New `read_service_name_robust` tries
   the `canonicalName`/`canonical` field, then reconstructs from the `name`+`parent` chain.
2. **`invoke_virtual` arg convention** — `invoke_virtual(receiver, name, desc, args)` prepends the
   receiver; `args` must be parameters **only**. The first cut passed the receiver inside `args` too,
   so `start()` got `context = svc` (the service) → `svc.getController()` → `NoSuchMethodError`. Fixed
   (also the `keySet`/`toArray` calls in `read_dep_names`).

## Verification
- **Regression pool 18/18 PASS** with the gate OFF (`RJVM=<worktree> run.sh`) — P1 is safe, no
  regressions. (`pool_gateoff2.log`.)
- **Gate ON, `testSubsystem`:** `IllegalStateException` at `AbstractControllerService.getValue:578`
  is **gone** (`getValue:578` count 0/2 runs). Trace: `install id=1 jboss.as.server-controller →
  start id=1 → [installs child id=3 model-controller-client-factory, id=4 notification-handler-registry]
  → start id=1 OK`. MCS reaches bci 507 (`putfield controller`) and returns. P1 held the service refs
  across the heavy `ModelControllerImpl` construction without a use-after-free.

## Session 2 — xnio gap RESOLVED (merged), new interpreter blocker found

**`ManagementWorkerService.installService` (the old "next gap") is FIXED** — three real fixes in
`native-builtins/src/xnio_async.rs` (commit on `fix/xnio` path, merged to dev), all always-on, **pool
18/18**:
1. `Options.<clinit>` shim populated a Rust store + getter methods but never wrote the real Java
   `public static final Option` fields → `getstatic Options.WORKER_IO_THREADS` was null → `Builder.set`
   NPE. Now also `set_static_field_by_name` each well-known option (+ added `CORK`).
2. `OptionMap` and `OptionMap$Builder` stored their Rust registry handle (a Long) in slot 0, which in
   real-JDK mode is the loaded class's **Object** field (`value` / `list`) — a Long there does not
   round-trip (read back as Object → handle 0 → "stale or unknown handle"). Moved the handle to a
   trailing extra slot (1), mirroring the `ServiceController._mscId` pattern; allocate 2 slots.
3. `Builder.set` / `set(boolean)` now tolerate a null `Option` (an unpopulated well-known static) by
   skipping it — the synthetic XnioWorker defaults options it doesn't read.

**New blocker (where `testSubsystem` now fails):** a VM **interpreter operand-stack overflow** —
`thread 'main-vm' panicked at vm/src/runtime/value_stack.rs:350: index out of bounds: the len is 24
but the index is 24` (`push_compact` past `max_size`). Identified via `CRATONVM_FRAME_TRACE=1`: it
fires during **`java.util.RegularEnumSet$EnumSetIterator`** iteration, which calls
`Long.numberOfTrailingZeros(J)I` → the interpreter **tail-call-optimizes** it (`[FRAME_TCO]`) into
`Integer.numberOfTrailingZeros(I)I`. Reached during the WildFly model boot (after MCS.start + the
worker install). This is a **general interpreter bug** (NOT MSC-specific, NOT my MSC code) exposed by
boot progress — `ntz` has a tiny `max_stack`, so the overflow is in a frame with `max_stack=24` near
the TCO / `stackless_cached` / `vcached2` cached-execution paths; the frame-trace depths interleave
(13–16 vs 35–36), pointing at nested invoke contexts from the MSC drive. `reset_for_tail_call`
(`frame.rs`) sets `max_stack` + `stack.clear()` but does NOT resize the reused `ValueStack`'s
`max_size` — a prime suspect, but the exact "why 24" needs an instrumented build (log each frame's
`max_stack` + the overflowing opcode). **This deserves its own focused effort; do not speculatively
change TCO (it is load-bearing) without root-causing.** Repro:
`CRATONVM_MSC_REAL_START=1 CRATONVM_FRAME_TRACE=1 bash run-health.sh` (grep the last `[FRAME_*]` before
`panicked`).

## Remaining work (current order — each blocks the next)
1. **Interpreter TCO / value-stack overflow** (the new blocker above) — must be fixed first; it
   currently aborts the VM mid-boot.
2. **P2 tail:** keep chasing whatever the boot surfaces after that until `testSubsystem` passes.
   Likely also needs **value injection** — `provides(name).accept(v)` → `requires(name).get()` —
   which is NOT wired (a dependent's injected `Supplier.get()` returns null). The `executorService`
   supplier survived so far because it's passed via the ctor, not MSC-injected.
3. **P4:** async services + standalone daemon to `WFLYSRV0025` + port 9990.
4. **P5:** `stop()` lifecycle; remove `CRATONVM_DBG_MSC` scaffolding before un-gating.

## Repro
- Build: `C:\craton\CratonVM-wfmsc\build-wfmsc.bat` (renamed toolchain survives parallel taskkill).
- testSubsystem (gate on + trace): `CRATONVM_MSC_REAL_START=1 CRATONVM_DBG_MSC=1 bash run-health.sh`.
- Pool (gate off): `RJVM=C:/craton/CratonVM-wfmsc/poolvm.exe bash <main>/test-infra/regression-pool/run.sh`.
