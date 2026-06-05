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

## Next gap (where `testSubsystem` now fails)
`org.jboss.as.server.mgmt.ManagementWorkerService.installService:71` →
`NullPointerException: null object argument`. `CRATONVM_DBG_NULL_NATIVE` shows a 3-arg native called
with `args[1] == null` (backtrace unsymbolized in the release strip build). `installService` does
`ServiceTarget.addService(SERVICE_NAME, new ManagementWorkerService(...)).setInitialMode(ON_DEMAND).install()`
(xnio worker). This is the start of the "iterate on natives the model boot surfaces" long tail the
plan anticipated — it is reached **only because boot now progresses past the controller assignment**.
To pinpoint the native, rebuild with `[profile.release] strip="none", debug="line-tables-only"` and
re-run with `CRATONVM_DBG_NULL_NATIVE=1`.

## Remaining work (per plan phases)
- **P2 tail:** chase surfaced natives (ManagementWorkerService null-arg first) until `testSubsystem`
  passes. Likely also needs **value injection** — `provides(name).accept(v)` → `requires(name).get()`
  — which is currently NOT wired (a dependent's injected `Supplier.get()` returns null). The
  `executorService` supplier survived here because it is passed via the ctor, not MSC-injected.
- **P4:** async services + standalone daemon to `WFLYSRV0025` + port 9990.
- **P5:** `stop()` lifecycle; remove `CRATONVM_DBG_MSC` scaffolding before un-gating.

## Repro
- Build: `C:\craton\CratonVM-wfmsc\build-wfmsc.bat` (renamed toolchain survives parallel taskkill).
- testSubsystem (gate on + trace): `CRATONVM_MSC_REAL_START=1 CRATONVM_DBG_MSC=1 bash run-health.sh`.
- Pool (gate off): `RJVM=C:/craton/CratonVM-wfmsc/poolvm.exe bash <main>/test-infra/regression-pool/run.sh`.
