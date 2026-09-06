# An `ArithmeticException` raised by compiled code carried an EMPTY stack trace — FIXED 2026-09-05

**Status:** FIXED, verified by a 300-run soak (0 failures, from 20% before) plus
`cargo test --workspace`.
**Files:** `vm/src/jit/helpers.rs`, `vm/src/runtime/interpreter.rs`,
`vm/src/runtime/interpreter/jit_bridge.rs`.
**Siblings:** `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`
built the machinery this record extends;
`jit-superseded-implicit-npe-leak-FIXED-20260903.md` is the other half of the
same signal's hygiene.

---

## 1. What was reported

`vm/tests/pgo02_guarded_virtual_inline.rs::test_pgo02_guarded_virtual_inline`
failed about one run in five, always with the same message:

```text
check_stack_trace_through_an_inlined_frame: the INTERPRETED trace names no
`tag` frame at all, so comparing it against the compiled one proves nothing.
The fixture or the capture path changed.
```

Measured before touching anything: **8/10 pass, and the same two iterations
fail** on a re-run — a stable ~20%, not host noise. (This mattered. The same
vector had already been written off as a load flake once, on one sample each
way.)

## 2. What it actually was

The check reads the trace the VM *captured* for an `ArithmeticException` raised
by `Divider.tag`'s `x / divisor`. In the failing runs that trace was not short —
it was **empty**:

```text
[sttrace] STORE hash=1 obj=0x7c1f…00d0 frames=2 names=["…callDivider", "…$Divider.tag"]
[sttrace] STORE hash=2 obj=0x7c1f…01b8 frames=0 names=[]
[sttrace] STORE hash=3 obj=0x7c1f…0248 frames=0 names=[]
```

`hash=1` is the first exception, raised while `callDivider` was still
interpreted. Every exception after it — i.e. every one raised once the method
was compiled — stored nothing. The `frames=2`/`frames=0` split is the whole
defect, and it is the same one the 2026-09-02 record describes for the NPE:

> An implicit NPE in compiled code is not thrown where it happens. […] the
> `java/lang/NullPointerException` is constructed afterwards, from the
> interpreter. `fillInStackTrace` therefore runs on a stack the compiled frames
> have already left.

A div-by-zero takes exactly that route: the `idiv`/`irem`/`ldiv`/`lrem` guard
jumps to a stub that calls `jit_throw_arithmetic`, which flags the signal,
returns the `i64::MIN` deopt sentinel and runs the method **epilogue**. The
throwable is built later, from the interpreter, on a stack with no compiled
frames on it.

The NPE half of that was fixed on 2026-09-02 with three pieces: a snapshot taken
inside the helper, delivered to every door that constructs the throwable, and a
trap-site id so the recovered frame carries a line. **Only the NPE half.** The
arithmetic signal had no snapshot, and none of the six doors attached one.

## 3. Why it was intermittent rather than always red

Whether a given `vm.invoke("callDivider", …)` ENTERS the compiled artifact is
timing-dependent — the artifact is installed by a background worker. A call that
ran interpreted produced a 2-frame trace and the check passed; a call that
entered compiled code produced an empty one and it failed. Same binary, same
input, ~20%.

## 4. The five parts of the fix

1. **Snapshot at the trap.** `jit_throw_arithmetic` now calls
   `snapshot_trap_frames(0)` while the compiled frames are still on the stack —
   the only moment both the signal and the frames exist. Trap key `0`: unlike an
   inline null check, a div-by-zero guard is reached from an opcode whose bci
   the frame walk already has, so there is no site id to pass and passing a
   wrong one would be worse than passing none.
2. **`materialize_implicit_signal`'s `Arithmetic` arm** attaches it, as its
   `Npe` arm already did. That function's own comment says attaching "in the
   constructor arm rather than at each door is what stops a fourth door from
   silently reopening it" — true, and the sibling arm was the fourth door.
3. **The compiled-callee door** drained the snapshot only for `ImplicitSignal::Npe`;
   it now drains it for `Arithmetic` too. This was never only a missing feature:
   an undrained snapshot stays in the thread-local cell, where the next take —
   belonging to a different throwable — finds it.
4. **The six drains.** Three in `jit_bridge.rs` (`execute_jit_call`,
   `execute_jit_call_decoded`, the one-shot door) and one in `interpreter.rs`
   each construct the `ArithmeticException` themselves; each NPE sibling
   attached, each arithmetic sibling did not. The `interpreter.rs` one is the
   door that was actually taken in the reproduction, and it was found only by
   labelling the other five, watching the flake reproduce with **no label
   printed**, and grepping for the drain again.
5. **`restash_jit_pending_arithmetic`.** With (1)–(4) in place the flake dropped
   to ~1% and the instrumentation then showed `ATTACH snapshot=None` after a
   `SNAPSHOT frames=2`: two whole-drain restore paths
   (`route_implicit_exc_through_callee`'s fallback and `restash_jit_signals`)
   put the NPE's snapshot back with its flag and restored the arithmetic flag
   **alone**, letting `DrainedJitSignals` drop the frames. The new function is
   the sibling of `restash_jit_pending_npe` and puts both back together.

Renamed with it, because the machinery is no longer NPE-specific:
`snapshot_npe_compiled_frames` → `snapshot_trap_frames`,
`take_jit_pending_npe_compiled_frames` → `take_jit_pending_trap_frames`,
`attach_snapshotted_npe_frames` → `attach_snapshotted_trap_frames`,
`JitSignals::npe_compiled_frames` → `trap_frames`. The kill switch keeps its
name (`CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT`) deliberately: renaming an env var
costs four inventory edits and buys nothing a doc line cannot.

## 5. Two test defects found on the way

Neither is the VM's fault and both were real.

**The stack-trace check's "interpreted" control was not interpreted.** Its doc
said "Measured against the SAME call before the method compiled", and
`check_uncaught_from_inlined_frame` — which runs first, on the same `Vm` —
leaves `callDivider` compiled and spliced. So the check compared the compiled
path against itself, and its own `interpreted == 0` floor is what caught that.
The check now drives `callDividerForTrace`, a second entry point onto the same
receiver that nothing else touches. Two checks sharing one entry point and one
`Vm` is the hazard; the floor is what kept it from being merely vacuous.

**`check_finally_runs_at_a_guard_eligible_site` counted `compiled_tally`'s own
calls.** It read the side-effect counter *after* `compiled_tally`, which keeps
CALLING the method while it waits for the background worker — up to 400 rounds
of 50. The assertion held exactly when the artifact was installed by the first
poll and reported "the `finally` ran 900 times for 700 calls" when it took four
rounds: an accusation of double-executing a `finally` where the extra runs were
the test's own. It surfaced only once the stack-trace check stopped failing
first and aborting the run — **a flake can hide a flake**. The counter is now
read before `compiled_tally`.

## 6. Verification

| binary | pass |
|---|---|
| before any change | 32/40, and 296/300 |
| snapshot + constructor arm + door (1–3) | 200/200, then 296/300 |
| + the six drains (4) | 250/300, all failures now `check_finally` |
| + restore paths (5) + both test fixes | **300/300**, and 300/300 again with the instrumentation removed |

`cargo test --workspace --no-fail-fast` on the same tree. `stack_trace_compiled_callee`
and `stack_trace_across_tiers` — the NPE half of this machinery — both still
pass, but note they complete in 0.00s on this host: they are probe-driven and
the probe is not staged here, so they SKIP. They are not evidence for this
change.

## 7. Known remaining gap: `ArrayIndexOutOfBoundsException`

`ImplicitSignal::Aioobe` has the identical hole and is NOT fixed here: no
`snapshot_trap_frames` call at any of its eight setters, and no attach in any of
the six doors. An AIOOBE raised by compiled code should therefore still carry an
empty trace.

It is left out deliberately rather than overlooked. Nothing in the tree witnesses
it, and this change is already five VM edits deep on a defect that had a witness;
the recipe is the same five parts and is worth doing with a test that can see it
fail first. The `restash_jit_signals` arm is written so the AIOOBE case slots in
without re-deciding who owns the single `trap_frames` slot.
