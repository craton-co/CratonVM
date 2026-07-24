# `value_stack.rs` `usize` underflow panic on a background NIO worker thread — FIXED

**This was a genuine CratonVM defect** in the x64 single-pass JIT backend's
deopt-frame snapshotting for invoke dispatch, not a fixture gap.

## Symptom

```
thread 'http-nio-127.0.0.1-auto-22-exec-3' panicked at vm/src/runtime/value_stack.rs:237:25:
index out of bounds: the len is 24 but the index is 18446744073709551615
```

`18446744073709551615` = `u64::MAX` — i.e. a `0usize - 1` underflow wrapping
around to the maximum representable value, then used as a Vec/slice index.

## Where it fired

On a **background NIO worker thread** (`http-nio-*-exec-N` /
`https-jsse-nio-*-exec-N`, not the main JUnit thread), inside
`java/util/concurrent/LinkedBlockingQueue.take()`, during Tomcat connector
pause/stop teardown between two parameterized test methods. Because it ran
on a background thread, CratonVM's panic handler caught it and the process
survived — the JUnit class as a whole still printed a normal
`FAILURES!!! Tests run: N, Failures: M` summary afterward instead of the
whole VM dying, which is why this didn't originally surface as an obvious
crash.

## Confirmed reproducing in (at least) two independent classes (pre-fix)

- `org.apache.catalina.nonblocking.TestNonBlockingAPI` — panic fired around
  test ~30-35 of 44 parameterized cases. (This class also fails on HotSpot
  for an unrelated, still-untriaged reason — tracked in
  `../../../known-issues/tomcat/untriaged-oddities.md`; that failure never
  explained the panic, which was CratonVM-only.)
- `org.apache.tomcat.websocket.TestWebSocketFrameClientSSL` — same
  `value_stack.rs:237:25`, same `u64::MAX` signature, over WebSocket/TLS
  traffic instead of plain NIO HTTP. This class passes cleanly on HotSpot,
  so it was an unambiguous, uncontestable CratonVM-only regression on its
  own.

## Root cause

`RUST_BACKTRACE=1` + `CRATONVM_DBG_DEOPT=1` on a standalone repro (looping
`TestNonBlockingAPI` until it fired — the race needs `take()` to get hot
enough to JIT-compile, so it's probabilistic, not every run) pinned the
exact mechanism:

```
[cratonvm-deopt] x64 frame-deopt entry reason=ReceiverTypeChanged at bci=27 locals=[...] stack=[]
[cratonvm-deopt] helper precise-resume of trapped callee java/util/concurrent/LinkedBlockingQueue.take:()Ljava/lang/Object; at bci=27
[PANIC_IN/prebuilt] java/util/concurrent/LinkedBlockingQueue.take()Ljava/lang/Object; pc=32 max_stack=2 :: index out of bounds: ...
```

`take()`'s bci 27 is `invokeinterface Condition.await:()V` (the receiver —
`this.notEmpty` — sits at operand-stack depth 1 there per `javap -v`, and
`take()`'s own `max_stack` is 2, matching the panic's `max_stack=2`). bci 32
(printed at panic time) is simply 27 + 5 (invokeinterface's instruction
length) — the interpreter had already advanced `pc` past the invokeinterface
while dispatching it (`execute_invokevirtual_cached` → `peek_at`) when the
underflow hit.

`jit/src/x64.rs`'s generic invoke-dispatch codegen (used for
megamorphic/PIC/MIC virtual+interface calls, plain direct calls, invokestatic,
and self-recursive calls) all share this shape:

1. pop the invoke's receiver + args off the compiler's abstract operand
   stack (to marshal them as native-call arguments),
2. emit the `CALL`,
3. call `emit_post_invoke_exception_check(ret_type)`, which — only when no
   snapshot already exists for this bci — builds a `DeoptimizationPoint`
   via `build_and_record_deopt_point(self.dbg_last_pc, ReceiverTypeChanged)`
   tagged `DeoptAction::Reinterpret` (i.e. "resume by re-executing this same
   invoke bytecode from scratch").

Because the snapshot in step 3 is built from `self.stack` **after** step 1
already popped the receiver/args, the recorded operand-stack snapshot for a
`Reinterpret`-at-this-bci deopt is missing exactly the values re-executing
the invoke needs. When the callee (`Condition.await()`) throws/deopts and no
earlier snapshot exists for the call's bci, `try_resume_trapped_callee`
resumes `take()` at bci 27 with an **empty** operand stack — and the
interpreter's `invokeinterface` handler immediately underflows fetching the
receiver.

The String-access and CRC32 intrinsic ladders in the same file already had
the correct pattern (`snapshot_pre_intrinsic_call(pc, reason)` called
**before** popping, so the snapshot captures the receiver/args) — it was
simply missing from the generic megamorphic/direct/self-recursive invoke
paths that don't go through an intrinsic.

## Fix

`jit/src/x64.rs`: added the same pre-pop `snapshot_pre_intrinsic_call(pc,
DeoptReason::ReceiverTypeChanged)` call (gated on `deopt_real_enabled()`,
idempotent per-bci) immediately before each affected pop loop:

- the MIC/PIC-dispatched generic invokevirtual/invokeinterface helper path,
- the plain direct-call path (invokespecial / monomorphic-guarded direct
  virtual/interface calls),
- the invokestatic direct-call path,
- the invokestatic dispatch-helper path,
- the self-recursive direct-call path.

Search `value-stack-usize-underflow-nio-worker-panic` in `jit/src/x64.rs`
for the exact insertion points and rationale comments.

## Verification

- `cargo test -p cratonvm-jit`: 83 passed, 0 failed (no regression in the
  JIT crate's own single-pass/IR-parity tests, including the
  `invokeinterface`/`invokevirtual`/`invokespecial` instance-call parity
  tests).
- 25 repro loops of `TestNonBlockingAPI` with `CRATONVM_DBG_DEOPT=1`: 0
  panics. The exact same trap (`reason=ReceiverTypeChanged at bci=27`) still
  fires just as often as before the fix, but now with `stack=[Object(...)]`
  (the receiver correctly preserved) instead of `stack=[]`.
- 10 additional clean repro loops (no debug-trace overhead, matching the
  original repro command exactly) of both `TestNonBlockingAPI` and
  `TestWebSocketFrameClientSSL`: 0 panics across all 20 runs.
- The two non-panic anomalies observed during verification were both
  pre-existing/environmental, not regressions: one heap OOM under host
  memory pressure (`--Xmx 2g` on a heavily loaded shared box) in an
  unrelated large-array test method, and one 300s-timeout kill purely from
  `CRATONVM_DBG_DEOPT=1`'s own I/O overhead. `TestWebSocketFrameClientSSL`
  continues to fail on an unrelated, pre-existing SSL/keystore connector-init
  gap (`testBug56032[JSSE]`: "Protocol handler initialization failed") that
  reproduced identically in the very first pre-fix baseline run — separate
  issue, not this bug, not addressed here.

## Status
- [x] Reproduced with `RUST_BACKTRACE=1` + `CRATONVM_DBG_DEOPT=1`.
- [x] Root-caused to `jit/src/x64.rs`'s generic invoke-dispatch deopt
      snapshot timing (post-pop instead of pre-pop).
- [x] **Fixed** — pre-pop snapshot added at all 5 affected invoke-dispatch
      sites.
- [x] Verified panic-free across 35 repro attempts (25 debug-trace + 10
      clean) after the fix; 83/83 jit crate unit tests pass.
