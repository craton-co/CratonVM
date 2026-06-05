# Implementation plan — make MSC `ServiceContainer` run real `service.start()`

**Owns:** `bug-wildfly-msc-service-start-callback.md` (testSubsystem `IllegalStateException` +
standalone `TIMEOUT_NO_READY`, and broadly WildFly daemon startup).
**Primary file:** `native-builtins/src/jboss_msc.rs` (+ `vm/src/memory/roots.rs`, `vm/src/memory/gc.rs`).
**Goal:** when an MSC service's dependencies are satisfied, invoke its real Java
`service.start(StartContext)` callback (today `run_start_local` fakes the `Up` transition),
so `ModelControllerService.start()` assigns `controller` and `HttpManagementService.start()`
binds port 9990.

This is a bounded but multi-part change. Each hard problem below has a proven template in-tree.

---

## Current state (what exists)
- `ServiceContainer` Rust state machine: `add_service` (line ~369), task queue, `run_start_local`
  (~519, **fakes Up, no callback**), `demand`, async-pending plumbing (`complete_async` ~452,
  `native_start_context_asynchronous` ~1159, `native_start_context_complete` ~1172).
- The one entry with a `NativeContext`: `native_service_container_add_service` (~1022), which
  stores the service as a **raw `usize` ptr** (`service_obj = o.as_ptr() as usize`, ~1044) and
  calls `drain_tasks_locally()` (~1078) — the T19.2 TODO.
- `StartContext` synthetic layout already reserved (comment ~813: "1 field: 0=controller_id").

## The 5 problems and their fixes

### P1 — GC roots for held service objects  *(do this FIRST; it's the foundation)*
Store the service as an `ObjectRef`, not a `usize`. Objects referenced only from the global
container must be GC roots + remap targets, exactly like the existing process-global caches:
- **Root scan:** add `gc_scan_msc_service_roots(&mut roots)` and call it from
  `vm/src/memory/roots.rs` (next to `lang_math::gc_scan_value_of_cache_roots` at line ~202 and the
  collections/ClassLoader scans ~211-222). It walks the container and pushes every live
  `service_obj` (and any held `StartContext`/controller-mirror refs).
- **Remap:** add the mirror re-point step in `vm/src/memory/gc.rs` (next to the step-15/16 re-point
  blocks ~270-291) so after a moving GC the stored `ObjectRef`s are updated to their new addresses.
- The container is in `native-builtins`; expose a `pub fn gc_scan/gc_remap` over `global_container()`
  (mirror `lang_math::gc_scan_value_of_cache_roots`). Use a side lock-free snapshot to avoid
  deadlock if GC runs while the container lock is held.

### P2 — Invoke the real `start()` (the behavior change)

**⚠ Correct interception point (verified 2026-06-05):** modern WildFly does NOT call the legacy
2-arg `ServiceContainer.addService(ServiceName, Service)` (the only install native today, line
~1561). `AbstractControllerService.start()` (and every subsystem) installs via the **ServiceBuilder
API**:
```
ServiceTarget.addService(ServiceName)           -> ServiceBuilder   (1-arg)
ServiceBuilder.provides(ServiceName...)          -> Consumer<V>      (value publisher)
ServiceBuilder.setInstance(Service)              -> ServiceBuilder
ServiceBuilder.install()                         -> ServiceController   <-- HOOK HERE
```
All of these currently run **real bytecode** (no natives). The behavior change must therefore
intercept **`ServiceBuilder.install()`** (or `ServiceBuilderImpl.install`), where the service
object is already attached (via `setInstance`) and the name (via `addService`). A first attempt
that hooked the legacy 2-arg `ServiceContainer.addService` was a no-op (that path is never taken).
Sub-steps: register a native for `org/jboss/msc/service/ServiceBuilder.install()` (and/or the impl
class); read the service object + name + provided names out of the builder's instance fields
(reverse `ServiceBuilderImpl` layout via `javap -p`); register the service in the Rust container;
then run the ctx-driven drive below. The `provides()` `Consumer` is how the service publishes its
value (e.g. the `controller`) — `Consumer.accept(value)` must store the value so dependents (and
`getValue`) see it.

Then replace the fake transition with a **ctx-driven drive** (the `try_real_start` helper drafted
on branch `fix/wildfly-finish` is the right drive logic — it just needs the right trigger):
```rust
fn drive_starts(ctx: &mut dyn NativeContext, container: &ServiceContainer) {
    loop {
        // pop one ready Start id WITHOUT holding the lock across the invoke
        let (id, svc_ref) = match container.take_ready_start() { Some(x) => x, None => break };
        let sctx = build_start_context(ctx, id);                    // P3
        let _ = ctx.invoke_virtual(svc_ref, "start",
                  "(Lorg/jboss/msc/service/StartContext;)V", &[Value::Object(Some(svc_ref)),
                   Value::Object(Some(sctx))]);
        container.finish_start(id);   // -> Up, unless async_pending; then schedule_dependents
    }
}
```
`take_ready_start` returns a Down/New service whose deps are all `Up` (`can_start`, ~612) and its
live `ObjectRef`. Loop is **iterative**, not recursive — re-entrant `addService` calls made by a
running `start()` just enqueue more tasks that the same loop drains. Never hold `inner` lock across
`invoke_virtual`.

### P3 — `StartContext`
Allocate the synthetic `org/jboss/msc/service/StartContext` (layout already reserved) with
`controller_id`. Wire the methods services actually call: `getController()` (return the controller
mirror), `complete()` / `failed(StartException)` (→ `complete_async` / mark failed),
`asynchronous()` (set `async_pending`), `getChildTarget()` if needed. Most are already stubbed
(`native_start_context_*` ~1159-1185) — extend to honour `getController`.

### P4 — Async services
`HttpManagementService` and others call `StartContext.asynchronous()` then `complete()` from a
worker. With the synchronous drive: if `start()` called `asynchronous()`, leave the service
`Starting` and return; it completes when the service later calls `complete()` (which already
transitions to `Up` + fires dependents). Verify the socket-bind path actually reaches `complete()`.

### P5 — Re-entrancy / ordering / stop
- Ordering: `take_ready_start` only returns services whose deps are Up, so out-of-order `addService`
  is fine — a dependent is picked up once its provider transitions to Up and `schedule_dependents`
  re-enqueues it; the drive loop re-checks. Services added before deps wait (correct).
- `stop()`: mirror `start()` for `Task::Stop` (invoke `service.stop(StopContext)`); lower priority.

---

## Phased delivery (each phase independently buildable + testable)
0. **P1 only** — store `ObjectRef` + GC root/remap. No behavior change (still fakes Up). Verify
   pool 15/15 + daemon still WFLYSRV0049 (proves the GC plumbing is inert/safe).
1. **P2 + minimal P3** — drive real `start()` synchronously. **Target:** testSubsystem
   `getValue()` returns a non-null controller (ISE gone). Expect to surface new natives the model
   boot needs — iterate.
2. **P3 full + P4** — StartContext + async. **Target:** standalone daemon reaches `WFLYSRV0025`
   and port 9990 accepts a connection.
3. **P5** — `stop()` lifecycle + cleanup; remove `CRATONVM_DBG_WFBOOT` scaffolding.

## Verification
- `testSubsystem` passes (or advances to a clearly-different layer) — `RunIt` health harness.
- Daemon: `grep WFLYSRV0025` + `Test-NetConnection -Port 9990` succeeds.
- Regression pool 15/15 (run probes individually — the loop "REGRESS" is a known soak/taskkill flake).
- No new panics/hangs; `--stack-dump-on-timeout` to bound the daemon.
- Build in an **isolated worktree off dev** (never the main checkout); seed libffi; verify relink
  via a unique string literal.

## Risks / fallbacks
- `ModelControllerService.start()` is large → will exercise many natives; budget iteration for new
  gaps (this is the point — it advances real WildFly boot).
- Deep re-entrancy → keep the drive loop iterative; cap depth + log if exceeded.
- If async never completes for a service, the daemon hangs at a *new* point (progress, not the old
  TIMEOUT) — diagnose with the now-working stack traces.
- Keep each phase on its own branch; merge to dev only after pool 15/15 + boot verification.

## Effort
Realistically a focused multi-session effort: P1 ~0.5d, P2+P3 ~1-2d (plus whatever natives the model
boot surfaces), P4 ~0.5-1d. The payoff is the single biggest unlock for WildFly-on-CratonVM.
