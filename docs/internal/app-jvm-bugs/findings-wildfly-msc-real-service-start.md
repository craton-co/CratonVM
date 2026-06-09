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

## Session 3 — TCO / value-stack overflow RESOLVED (merged)

Root cause: `Frame::reset_for_tail_call` (frame.rs) resized the reused frame's **locals** for the
tail-called method but only `clear()`-ed the operand stack — it kept the old `ValueStack`'s `max_size` +
backing Vecs. A tail call into a method with a **larger `max_stack`** then overflowed the smaller reused
stack (`RegularEnumSet$EnumSetIterator` → `Long.numberOfTrailingZeros` TCO→`Integer.numberOfTrailingZeros`).
Fix: added `ValueStack::ensure_max_size` (grow-only — never shrinks a live stack) and call it in
`reset_for_tail_call`, mirroring the locals resize already there. General interpreter correctness fix.
Pool clean (the 3 `*-modload` "REGRESS" are **path-only baseline staleness** — dev relocated the pool to
`apps/probe/test-infra/regression-pool/`; baselines embed the old `...\test-infra\...` path, lines 3-4
`Loaded module`/`OK` match; affects any binary, not this change).

**New blocker:** boot now reaches **real ModelController op processing** (`TRACE WFLYCTL0161: Operation
succeeded, committing`), then `java.util.ResourceBundle.checkNamedModule` → `getCallerModule(Class).isNamed()`
throws `NoSuchMethodError: java/lang/String.isNamed()Z` — i.e. `Class.getModule()` (synthetic Module;
`lib.rs` ~2979 / `phases_late.rs` ~15743 alloc a `java/lang/Module` with field0 = name String) handed
back a **String** as the `isNamed()` receiver. **Nondeterministic** — a `CRATONVM_FRAME_TRACE` run instead
pushed the correct `java/lang/Module.isNamed()`. Suspect a **GC stale-ref / reused-slot** on the synthetic
Module (cf. `reference_classloader_gc_root_gap`) and/or synthetic-Module field layout vs real
`java.lang.Module`. Investigate with the heap/frame stale-ref tooling, not a quick patch.

## Session 4 — `String.isNamed` RESOLVED (merged); now processing mgmt operations

NOT a GC stale-ref (large heap didn't help). Real cause: `ResourceBundle.getCallerModule` returns
`caller.getModule()` only when `Reflection.getCallerClass() != null`, else
`getSystemClassLoader().getUnnamedModule()`. `ClassLoader.getUnnamedModule()` ran **stock bytecode**
reading the synthetic loader's never-populated `unnamedModule` field → a String receiver for `isNamed()`.
Fix (merged): registered `ClassLoader.getUnnamedModule()` (classloader_real.rs) returning a synthetic
**unnamed** Module (field0 = null → the `Module.isNamed()` native = false → `checkNamedModule` passes;
single alloc, GC-safe) + pinned `m_obj` across `create_string` in both `Class.getModule` natives. Pool
15/18 (3 `*-modload` = path-only baseline staleness from the `apps/probe/` pool move).

**New blocker:** boot now PROCESSES management operations (`WFLYCTL0161` committed / `WFLYCTL0013`
failed), then `NoSuchMethodError: org/jboss/as/controller/ControlledProcessState.ordinal()I`.
`ControlledProcessState` is a regular class (`state: AtomicStampedReference<State>`); its nested `State`
is the enum with `ordinal()`. So `.ordinal()` is being called on a `ControlledProcessState` instead of
`getState()`'s `State` enum — a type confusion (suspect the ControlledProcessState shim /
`AtomicStampedReference.getReference()` returning the wrong object).

## Session 5 — `ControlledProcessState.ordinal` + `EnhancedQueueExecutor` hang RESOLVED → testSubsystem now COMPLETES

- **`ControlledProcessState.ordinal`:** `native_process_state_get_state` (wildfly_core.rs) allocated a
  `ControlledProcessState`, but `getState()`'s return type is `ControlledProcessState$State` (the enum),
  so `x.getState().ordinal()` / switch-maps (`ModelControllerImpl$4`) hit
  `NoSuchMethodError ControlledProcessState.ordinal()I`. Fixed: return the real `State` enum-constant
  singleton (map our `ProcessState` → JDK constant by NAME — ordinals differ — via
  `static_field_index_by_name` + `get_static_field`).
- **`EnhancedQueueExecutor` cleanup hang:** the synthetic Rust-backed executor never maintains its
  `threadStatus` long, so stock `shutdown()` spins forever in `compareAndSetThreadStatus`
  (`AtomicLongFieldUpdater.compareAndSet`) during the `@After` cleanup. Located with
  `--stack-dump-on-timeout 75` (the harness disables it with `0`). Fixed: shim
  shutdown()/shutdown(Z)/isShutdown/isTerminated/awaitTermination to terminal values.

**MILESTONE:** `testSubsystem` + `testSchema` now **run to completion** (`Tests run: 2`) instead of
crashing/hanging. `testSubsystem` fails as a normal `RuntimeException` at
`SubsystemTestDelegate.validateDescriptionProviders:470` — a WildFly **model-validation** failure, a
different category from the (now-cleared) VM crashes/hangs.

## Remaining work (current order — each blocks the next)
1. **`validateDescriptionProviders` RuntimeException** — a boot op (`WFLYCTL0013`) failed during the
   subsystem `:add`. The failure description is HIDDEN by a jboss-logging gap (`WFLYCTL0013` logs literal
   `%s`, args unsubstituted). Fix the `%s` substitution (or surface the op-failure another way) first,
   then diagnose the health-subsystem op / description-provider mismatch.
2. **P2 tail / value injection** — `provides(name).accept(v)` → `requires(name).get()` is NOT wired (a
   dependent's injected `Supplier.get()` returns null); the `executorService` supplier survived only
   because it is ctor-passed, not MSC-injected.
3. **P4:** async services + standalone daemon to `WFLYSRV0025` + port 9990.
4. **P5:** `stop()` lifecycle; remove `CRATONVM_DBG_MSC` scaffolding before un-gating.

## Repro
- Build: `C:\craton\CratonVM-wfmsc\build-wfmsc.bat` (renamed toolchain survives parallel taskkill).
- testSubsystem (gate on + trace): `CRATONVM_MSC_REAL_START=1 CRATONVM_DBG_MSC=1 bash run-health.sh`.
- Pool (gate off): `RJVM=C:/craton/CratonVM-wfmsc/poolvm.exe bash <main>/test-infra/regression-pool/run.sh`.
