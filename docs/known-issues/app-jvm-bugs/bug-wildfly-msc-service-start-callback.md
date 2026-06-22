# WF — MSC `ServiceContainer` never invokes the real Java `service.start()` callback

**Status:** ⏳ OPEN (precisely root-caused; the fix is a substantial MSC-lifecycle undertaking)
**Affects:** WildFly health `testSubsystem` **and** the standalone daemon `TIMEOUT_NO_READY` (mgmt port) — and, by extension, essentially every WildFly/jboss-msc daemon scenario that relies on a service actually *running* its `start()` logic.
**Files:** `native-builtins/src/jboss_msc.rs`

---

## Symptoms

1. **health `testSubsystem[0]`** → `java.lang.IllegalStateException` thrown at
   `org.jboss.as.controller.AbstractControllerService.getValue(AbstractControllerService.java:578)`.
   `getValue()` is simply:
   ```java
   public ModelController getValue() {
       if (controller == null) throw new IllegalStateException();   // line 578
       return controller;
   }
   ```
   The `controller` field is **null** — it is assigned inside
   `AbstractControllerService.start(StartContext)`, which **never ran**.
   Call chain (innermost last; the on-dev stack-trace fix prints it reversed):
   `getValue:578` ← `ModelTestKernelServicesImpl.<init>:71` ← `AbstractKernelServicesImpl.<init>/create` ←
   `MainKernelServicesImpl.<init>:63` ← `SubsystemTestDelegate$KernelServicesBuilderImpl.build:557` ←
   `AbstractSubsystemBaseTest.standardSubsystemTest:219` ← `testSubsystem`.

2. **standalone daemon `TIMEOUT_NO_READY`** → boots to `INFO WFLYSRV0049 … starting` and
   `[cratonvm] main() returned`, but the HTTP management endpoint (port 9990) never opens, so
   the server never reaches `WFLYSRV0025 … started`. `WFLYSRV0049` is logged *before* services
   start; the management service's `start()` (which binds the listener) never runs.

Both are the **same** root cause.

## Root cause

CratonVM intrinsifies `org/jboss/msc/service/ServiceContainer` with a Rust state machine
(`native-builtins/src/jboss_msc.rs`). When a service is installed and its dependencies are up,
the container schedules a `Task::Start`, processed by `ServiceContainer::run_start_local`:

```rust
// jboss_msc.rs ~line 519
fn run_start_local(&self, id: u64) {
    // … Down -> Starting …
    // Since we have no real Java callback to run locally, just
    // transition to Up and fire dependents.              // <-- line 540-541
    //   c.state = ServiceState::Up;
}
```

It transitions the service **straight to `Up` without ever invoking the Java
`service.start(StartContext)` callback.** So for `ModelControllerService` (whose `start()` parses
the boot ops, installs subsystems, and assigns the `controller` field) nothing happens →
`controller` stays null. For the daemon's management `HttpManagementService` (whose `start()`
binds the `ServerSocketChannel`) nothing happens → the port never opens.

The fake path is reached from the one native that *does* hold a `NativeContext`:

```rust
// jboss_msc.rs ~line 1075 (native_service_container_add_service)
// Run the task queue locally so the mirror's state reflects the
// transition before we return.  When real Java `start()` callbacks
// are wired in T19.2 this will shift onto the worker threads.
container.drain_tasks_locally();   // -> run_start_local (fake)
```

i.e. this is **documented-incomplete (the “T19.2” TODO)**, not an accident.

## Why the fix is non-trivial

To run the real callback the container must `ctx.invoke_virtual(serviceObj, "start",
"(Lorg/jboss/msc/service/StartContext;)V", [startCtx])`. Blockers:

1. **GC roots.** The container stores the service object as a raw `usize` pointer
   (`service_obj = o.as_ptr() as usize`, line ~1044). Objects held only in the process-global
   container are **not GC roots**, so after a young GC the pointer dangles (same class of bug as
   `reference_classloader_gc_root_gap`). Driving `start()` later (on a worker, or after any
   allocation) would be a use-after-free. A correct fix must register the held service objects as
   GC roots / remap targets.
2. **Re-entrancy.** `ModelControllerService.start()` installs many more services → re-entrant
   `addService` calls into the container. The drive loop must not hold the container mutex across
   the `invoke_virtual`, and must converge.
3. **Async lifecycle.** `StartContext.asynchronous()` + `complete()` (already partially modelled,
   lines ~1159-1185) must be honoured — a service that goes async stays `Starting` until it calls
   `complete()` on a worker thread.
4. **Threading.** Real MSC starts on a small worker pool (`pool_cv`, line ~750). Synchronous
   in-`addService` invocation changes timing and risks deep recursion; doing it on workers needs
   those Rust threads to have VM/`NativeContext` access.
5. **Breadth.** `ModelControllerService.start()` boots the *entire* model — it will exercise many
   other natives; expect to surface further gaps.

This is the **central WildFly-on-CratonVM blocker**: until services run their `start()`, no
WildFly controller/management/subsystem-runtime path can complete. It deserves its own focused
effort (GC-root design + a real start-drive loop + `StartContext`), not a dangling-pointer hack
(which would violate the project's “real fix or clear error” rule and crash intermittently).

## Diagnosis aids
- The on-dev Throwable stack-trace fix (`bug-wildfly-throwable-stack-trace-capture.md`) makes the
  throw site visible — note the trace is currently emitted **outermost-first** (read the tail).
- `CRATONVM_DBG_WFBOOT` (added by the earlier wildfly-testsubsystem agent in `jboss_msc.rs` /
  `wildfly_core.rs`, not merged) traces the MSC add/start path and the `ControlledProcessState`
  shim.
