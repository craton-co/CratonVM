# JIT on-stack-replacement (OSR) re-executes loop iterations — silent, no exception, extra elements added to collections

**Status: FIXED on `dev`, branch `fix/jit-osr-loop-duplicate-execution-20260720`.**
**Root-caused and closed 2026-07-20 in the same session that found it. Confirmed root**
**cause: OSR-compiled methods containing `invokedynamic` can bail to the interpreter**
**AFTER already committing real loop side effects, and the "safe reject" fallback**
**silently resumes from the STALE pre-OSR state, re-executing them.**

## Symptom (as originally found)

A `for` loop that runs long enough to trigger CratonVM's back-edge
on-stack-replacement (OSR) compilation, in a method that also contains code
AFTER the loop, executed MORE iterations than the loop bound specifies — no
exception, no crash, just silently wrong results (extra elements appended to
a collection, an over-large counter, etc).

Minimal repro (still in the repo, now regression coverage — see
`docs/known-issues/repros/jit-osr-loop-duplicate-execution/`):

```java
List<Object> keys = new ArrayList<>(n);
for (int i = 0; i < n; i++) {
    keys.add(new Object());
}
System.out.println("n=" + n + " keys.size()=" + keys.size());   // <-- string concat = invokedynamic
```

**HotSpot:** `n=4000 keys.size()=4000` (correct, every run).
**CratonVM before this fix (JIT on, default):** `n=4000 keys.size()=6000` —
extra elements added, deterministic, reproduces single-threaded with no
concurrency involved.
**CratonVM after this fix:** `n=4000 keys.size()=4000`, matching HotSpot
across the whole originally-reported threshold table (100 through 15000, plus
a value-level diagnostic confirming no index is duplicated, not just that the
final count is right).

## Root cause (confirmed via `CRATONVM_DBG_OSR=1 CRATONVM_DBG_DEOPT=1` tracing)

1. The loop crosses the OSR back-edge threshold; OSR successfully enters the
   JIT-compiled continuation with the CURRENT live `i` and correctly runs the
   loop to completion (real, committed `ArrayList.add` calls for the true
   remaining range — this part was always correct).
2. Immediately after the loop, `System.out.println("..." + n + ...)`
   compiles to `invokedynamic` (Java 9+ indy-based string concatenation). The
   x64 backend's `0xba` codegen arm unconditionally lowers every indy call
   site to a `DeoptReason::UnreachedCode` trap (it never links/inlines the
   bootstrap) — this is intentional and, for a NORMAL method-entry JIT
   compile, is handled correctly (a fresh interpreter frame is built from the
   reconstructed state and resumed precisely).
3. For OSR specifically, the bail instead needs `transfer_osr_exit_into_live_frame`
   (`../../../vm/src/runtime/interpreter.rs`) to merge the JIT-advanced state into the
   PRE-EXISTING live interpreter frame. That merge failed for two reasons at
   the trap site:
   - The reconstructed LOCALS included `FrameValue::Unsupported` for the loop
     counter's slot, because `classify_local_kinds` (`../../../jit/src/x64.rs`) is a
     coarse WHOLE-METHOD scan: a JVM local slot used as more than one kind
     ANYWHERE in the method (here, `i` — `int` — and later `sum` — `long` —
     legally reusing the same slot after `i`'s scope ends) is always
     `Ambiguous`/`Unsupported`, even at bytecode offsets where the reused slot
     provably cannot be read yet (per the verifier's definite-assignment
     rule).
   - The reconstructed OPERAND STACK at the indy call site ALSO contained
     `Unsupported` entries (the live arguments about to be passed to the
     bootstrap) — a genuine, at-this-exact-point type/location ambiguity in
     the single-pass backend's per-site stack-slot classifier, unlike the
     locals case.
4. `transfer_osr_exit_into_live_frame` rejected the whole transfer
   ("unmappable local" / "unmappable stack slot"), and `try_osr`'s "safe
   reject" default then resumed interpretation at the STALE pre-OSR back-edge
   pc/locals — an invariant the code's own comments document as "correct only
   when the bail precedes any committed loop iteration." That invariant does
   NOT hold here: the loop already ran to completion inside the OSR'd
   continuation, so the interpreter silently re-executed (and
   re-committed) the whole already-done range.

## Fix (landed)

Two changes in `../../../vm/src/runtime/interpreter.rs`:

1. **`transfer_osr_exit_into_live_frame`** now tolerates a `FrameValue::
   Unsupported` LOCAL slot instead of rejecting the whole transfer: it simply
   leaves that slot's current live value untouched (safe, because bytecode
   that has passed verification can never read a local before a fresh
   `store` in its own scope — see the function's updated doc comment). Stack
   slots keep the strict all-or-nothing behavior (an operand-stack value is
   always about to be consumed, so there is no scope guarantee protecting a
   stale/fabricated value the way there is for a local).
2. **RBC.7** (new, alongside the existing RBC.6 `has_athrow` ban in
   `compile_osr_artifact`): never OSR-compile a method containing
   `invokedynamic`. This is the fix that actually closes the reported bug —
   the operand-stack ambiguity at an indy trap site can't be safely tolerated
   the way the local case can, so the method-entry (non-OSR) JIT compile path
   remains available and correct, exactly mirroring the existing precedent
   for `athrow`.

Fix #1 is an independent, genuine correctness improvement (it widens the set
of OSR-exit bails that can now transfer precisely instead of falling back to
"safe reject"); fix #2 is what makes THIS repro's specific bail path
unreachable, by construction, since it's a permanent property of the method's
bytecode. Together they close the bug for the exact reported case and for
any other method whose only unmappable-transfer obstruction is a coarse
whole-method local-slot ambiguity.

## Verification

- Repro (`LoopDupOsrRepro.java`) across n = 100/500/1000/1500/2000/2500/3000/
  4000/5000/10000/15000: all match HotSpot (`keys.size() == n`) after the fix;
  before the fix, n ≥ 2500 showed the overshoot (6000/8000/23000/... for
  4000/5000/10000).
- Value-level diagnostic (`LoopDupOsrDiag.java`, prints every list value and
  finds the first index whose value ≠ its own index): `firstMismatch=-1` for
  n=10000 after the fix (before the fix it found index 4000 holding `2000`,
  i.e. an exact repeat of the range added by the OSR'd continuation).
  `sum` also matches the closed-form `n*(n-1)/2` exactly.
- A no-`invokedynamic` variant (`LoopDupOsrNoIndy.java`, same loop + prints
  without string concatenation) never reproduced the bug even before the
  fix — confirms indy (not "more code after the loop" in general) is the
  specific trigger, consistent with the root cause above.
- Unit tests: existing `transfer_osr_exit_into_live_frame` coverage updated —
  `osr_exit_transfer_rejects_unmappable_without_mutating` (which asserted the
  OLD, buggy all-reject behavior for an `Unsupported` local) replaced with
  `osr_exit_transfer_tolerates_unmappable_local` (asserts the new tolerate-
  and-skip behavior) plus a new `osr_exit_transfer_rejects_unmappable_stack_
  without_mutating` (confirms the stack side still rejects strictly, as
  designed). `cargo test --release -p cratonvm-vm --lib runtime::interpreter`:
  194/195 passed (the one pre-existing failure,
  `buffered_input_stream_real_jdk_uses_its_own_bytecode`, reproduces
  identically on unmodified `dev` and is unrelated — flagged separately).
- Broader `cargo test --release -p cratonvm-vm --lib`: 2219 passed / 19
  failed, all 19 confirmed pre-existing/environment (mostly `runtime::
  lock_order` tests that assert `cfg!(debug_assertions)` and only run
  meaningfully in a debug build, plus the same unrelated `jit::skip_list`/
  `native::jni`/`BufferedInputStream` failures already present on `dev` in
  files this change never touches).

## Related bugs checked (per this doc's own "why this matters" note)

Searched `../../known-issues`/`..` for "extra"/"duplicate"/"more
than expected" symptom docs that might share this root cause:

- `docs/internal/spring-boot-probe-sweep/nested-try-catch-jit-divzero-rerun.md`
  — a DIFFERENT, already-fixed JIT double-execution bug (integer div-by-zero
  uncommon-trap re-running the WHOLE method in normal, non-OSR JIT compiles).
  Same general family ("uncommon trap re-runs re-execute side effects") and
  fixed the same way in spirit (direct-throw / precise resume instead of a
  blind re-run), but a distinct code path and already closed.
- `../gaps/bc-jit-miscompile-handoff.md` section A — a
  superficially similar "OSR loop overrun" (BouncyCastle `Horst.horst_sign`),
  but a completely different, already-fixed root cause: a 1-byte
  `bytecode_len_at` miscount for `ldc` that mis-stepped the PC-stepping
  precompute and left a loop-exit branch dead-code-eliminated. Unrelated to
  OSR-exit transfer.
- The STOMP/WebSocket "duplicate CONNECT frame delivery" item in
  `CRATONVM-SPRING-GENUINE-BUGLIST.md` is a network/transport-layer
  redelivery issue, not JIT re-execution — unrelated.

No other open bug matching the "too many X" shape was found to share this
root cause.
