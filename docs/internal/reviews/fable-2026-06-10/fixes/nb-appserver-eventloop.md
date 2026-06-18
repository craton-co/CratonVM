# Fix nb-appserver-eventloop — event-loop drops submitted Runnables (B1)

## Finding

**B1 (critical)** — the Netty/Vert.x event loop (`vertx_eventloop.rs`) and the
XNIO I/O thread (`xnio_io_thread.rs`) accept a Java `Runnable` via
`execute()` / `submit()` / `schedule()` but **never actually run it**. The
submit path enqueued a Rust no-op closure (`Box::new(|| {})`) — or, in XNIO, a
closure that only flagged `mark_synthetic_runnable_ran(ptr)` — and dropped the
real `Runnable` on the floor. Any Netty/Vert.x/XNIO code that relies on
`eventLoop.execute(...)` to run work (almost all of it) silently did nothing,
so callbacks never fired.

This is a real correctness bug, **not** a stub, and was even mis-tagged
`NativeKind::Bridge`.

## Root cause

The event loops carry their work queue as `Box<dyn FnOnce() + Send + 'static>`
Rust closures that fire on a **pinned OS thread that has no `NativeContext`**
(`run_vertx_event_loop` / `run_io_loop`). A Rust closure on that thread
structurally cannot drive a Java `Runnable.run()` — there is no interpreter
context reachable there. So the original code "solved" the type problem by
enqueuing a no-op and leaving a comment that the real invocation "comes from
the VM interpreter dispatching `run()` on this event-loop thread" — but nothing
ever did that dispatch, so the task was simply lost.

The native methods themselves (`native_nel_execute`, `native_iot_execute`,
`native_nel_submit`) **are** called by the interpreter and **do** hold a live
`ctx`. The correct fix mirrors the MSC pattern in
`jboss_msc.rs::drive_starts` (line ~1744), which invokes `Service.start()`
synchronously via `ctx.invoke_virtual` from inside the native call. We run
`Runnable.run()` there, where the context exists, instead of deferring an
impossible-to-execute closure to the carrier thread.

## Exact change

`native-builtins/src/vertx_eventloop.rs`
- `native_nel_execute` (`execute(Ljava/lang/Runnable;)V`): bind the validated
  non-null `Runnable`, then `ctx.invoke_virtual(runnable, "run", "()V", &[])`
  instead of `el.schedule_task(Box::new(|| {}))`. `execute()` is fire-and-forget
  per the Netty contract: a task that throws is logged
  (`tracing::warn!`) and **not** propagated to the submitter (returns `Ok(None)`).
  Bumps `el.stats.tasks_run` so dispatch observability stays honest.
- `native_nel_submit` is unchanged in body — it delegates to
  `native_nel_execute` then returns a `Future`, so it inherits the fix.
- `native_nel_schedule`: for `delay == 0` (the common "run ASAP on the loop"
  case, e.g. `schedule(r, 0, MILLISECONDS)`) invoke `run()` now via the
  interpreter. For a real future delay, keep the timer registration (returns a
  working `ScheduledFuture`) rather than running synchronously now — running
  now would violate the delay contract. Deferred Java dispatch on the carrier
  is called out as a follow-up.

`native-builtins/src/xnio_io_thread.rs`
- `native_iot_execute` (`org.xnio.XnioIoThread.execute(Ljava/lang/Runnable;)V`):
  same fix — invoke `Runnable.run()` via `ctx.invoke_virtual`. Retains the
  `mark_synthetic_runnable_ran` / `record_synthetic_pending` test-observability
  hooks, and keeps queue-cap rejection (`RejectedExecutionException`) by reading
  `handle.pending_len`. Fire-and-forget error handling identical to Netty.
- `executeAfter` / `executeAtTime` (deferred, return a `Key`) left registering
  the scheduled timer — same deferred-dispatch follow-up as Vert.x `schedule`.

`native-builtins/src/xnio_async.rs`
- **No change needed.** This file is `OptionMap` / `Options` / `IoFuture` —
  config + futures, not a Runnable event loop. Its `IoFuture$Notifier`
  dispatch already invokes the real Java method via `ctx.invoke(...)`
  (`fire_notifier`, line ~313). The "no-op" comments there are about
  *monotonic future-status transitions*, which are correct semantics.

### B9 (spawn_worker panics on thread-spawn failure)
Not present in my owned files. The report locates B9 at `wildfly_core.rs:404-408`
(not owned). The thread spawns in my files already propagate the error:
`xnio_io_thread.rs::spawn_io_thread_with_ctx` uses
`.map_err(|e| format!(...))?` (line ~932) and
`vertx_eventloop.rs::spawn_vertx_event_loop_inner` likewise (line ~746). No
`.expect()`/`panic!` on a production thread spawn here.

## Files touched
- `native-builtins/src/vertx_eventloop.rs` — `native_nel_execute`,
  `native_nel_schedule` bodies + 2 new regression tests.
- `native-builtins/src/xnio_io_thread.rs` — `native_iot_execute` body.
- `docs/reviews/fable-2026-06-10/fixes/nb-appserver-eventloop.md` (this note).

## Tests added
Two `#[cfg(test)]` tests in `vertx_eventloop.rs`:
- `nel_execute_invokes_runnable_and_swallows_task_error` — arms the mock's
  one-shot `invoke_virtual_result` to an `Err`, then asserts (a) `execute()`
  still returns `Ok` (fire-and-forget contract) and (b) the armed result was
  *consumed*, proving `Runnable.run()` was actually invoked (vs the old drop,
  which would leave it un-consumed).
- `nel_submit_invokes_runnable_and_returns_future_on_task_error` — same proof
  for `submit()`, plus the non-null `Future` return.

Both use the existing `*ctx.invoke_virtual_result.get() = Some(Err(...))`
pattern (see `lang_system.rs`), so they compile against the in-crate
`MockNativeContext`. Existing tests
(`nel_execute_enqueues_without_error`, `nel_schedule_returns_scheduled_future`,
`nel_submit_returns_future`) remain green because the default mock
`invoke_virtual` returns `Ok(None)`.

## Follow-up & risk
- **Synchronous execution semantics.** We run the task synchronously on the
  *calling* thread rather than truly "on the event-loop thread." This is correct
  for `execute()` ordering in the common case and unblocks callbacks, but a task
  that itself blocks waiting for the loop, or that strictly requires
  `inEventLoop()==true`, could behave differently than HotSpot. The deeper fix
  is a VM-thread-aware deferred dispatch from the carrier (the same gap as XNIO
  `set_channel_dispatcher` never being wired — report item #11, in
  `xnio_conduits.rs`, not owned).
- **Deferred delays.** `schedule(delay>0)` / `executeAfter` / `executeAtTime`
  still register a no-op timer and do not yet invoke the Java task on expiry
  (deferring to the contextless carrier remains impossible). They return a
  working `ScheduledFuture`/`Key`. Tracked as follow-up; out of scope for the
  immediate-`execute()` B1 fix.
- **Re-entrancy.** `invoke_virtual` runs arbitrary Java that could re-enter
  `execute()`. Each call is a fresh interpreter frame with its own root scan;
  no loop lock is held across the invoke (we read fields, drop the borrow, then
  invoke), so this matches the MSC `drive_starts` re-entrancy posture.
