# JIT round 14 proposals, lane trace6 (wave 7)

Ranked. Each builds on what wave 7 landed (TW6-2 join decline, TW6-3 two-site argument checks, RV6-2
receiver-NPE screen).

## TW7-1. Match HotSpot `-XX:-VMContinuations` on virtual threads

**What.** Page `r14w7-trace6-virtual-thread-frames-reference-20260929.md`: this VM's virtual threads
are `BoundVirtualThread`s, so the JDK bodies run their platform branches; `--jdk-only` already prints
HotSpot `-VMContinuations` rows, `--compatible` mixes (wait at 382, sleep with no frames). Route
virtual-thread captures through `native_standin_frames` behind a default-on switch.
**Benefit:** both modes print the same rows; `sleep` on a virtual thread gets its frames.
**Cost:** small (drop one screen). **Risk:** flips the wave-5 `virtualWait` row from HotSpot-default
382 to 389 under `--compatible`: needs the owner's call. **First step:** the owner picks the
reference.

## TW7-2. A typed served-call hint

**What.** `JvmThread::served_timed_join_hint: Option<bool>` now carries three meanings, told apart by
the throwable and call it is taken for (timed-join loop, join decline, argument-check site). Replace
it with `enum ServedCallHint { JoinLoop { timed: bool }, JoinDecline, ArgCheckSite(u8) }` and a
`NativeContext::set_served_call_hint` (default no-op) in `native-api/src/registry.rs`.
**Benefit:** the capture can refuse a hint of the wrong kind instead of reinterpreting the bit; a
third site becomes expressible. **Cost:** small, but in interpreter-round / API files
(`jvm_thread.rs`, `native-api`). **Risk:** none behavioural. **First step:** add the enum beside the
bool and migrate the three setters in `lang_system.rs`.

## TW7-3. Per-VM throwable class ids for a truly lock-free stand-in screen

**What.** `append_native_standin_frames` reads the throwable's class NAME from the class store before
every rule. With `SharedVm` holding the ids of `NullPointerException`, `InterruptedException`,
`IllegalArgumentException` and `IllegalMonitorStateException` (filled once at bootstrap), the
receiver-NPE refusal (RV6-2) and `is_standin_throwable` could run before the lock, which matters when
a `native_exc_init_*` served the constructor (then the trim never took it).
**Benefit:** NPE-heavy `--compatible` code stops taking the class-manager read lock per throw.
**Cost:** a per-VM field (not a global) and one bootstrap hook. **Risk:** loader identity: the ids must
be the bootstrap loader's classes. **First step:** a `CRATONVM_DBG_STTRACE` count of captures that
take the lock only in `append_native_standin_frames`.

## TW7-4. Other threads' stacks parked in a declined join

**What.** `append_parked_standin_entries` (T4-3) rebuilds the `join()` chain for another thread
parked in a served `join()`, with no hint. For a `java.lang.VirtualThread` target it would show the
`wait` chain the capture now declines. The parked thread's published stack cannot see the target;
the served join could publish it (a `JvmThread` field read by `thread_stack_trace`).
**Benefit:** consistency with TW6-2 once continuations exist. **Cost:** small. **Risk:** none today
(unreachable: no `VirtualThread` can be built). **First step:** only when continuation support lands.
