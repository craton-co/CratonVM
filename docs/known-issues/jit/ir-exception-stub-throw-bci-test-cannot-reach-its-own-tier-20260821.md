# `ir_exception_stub_stamps_this_methods_throw_bci` has stopped reaching the tier it tests

**Status: OPEN.** Found 2026-08-21 while running the full `cratonvm-vm` suite
against `perf/compiled-ldc-const-cache-20260820`, and confirmed **identical at
the merge base `eadd845c4`** — this is dev's, not that branch's.

```
test ir_exception_stub_stamps_this_methods_throw_bci ... FAILED
panicked at vm/tests/jit_ir_exception_stub_throw_bci.rs:291:
  `body` was never reported as compiled by the optimizing tier —
  this run proves nothing about the IR exception stub.
```

It is one failure in 3 683 tests across 52 binaries; everything else in the
crate is green in both arms.

## It is a vacuity guard firing, not a wrong answer

The test does not claim the stamp is broken. It claims it could not ask. Its own
precondition — that `IrExceptionStubThrowBciProbe.body` is compiled by the
OPTIMIZING (IR/C2) tier — is not met, and the probe says why in as many words:

```
[ir] admission IrExceptionStubThrowBciProbe.body(I[I)V: an inline trap this
     tier deopts on (pc=4, opcode=0x2e) sits in a protected range that also
     commits a side effect; the deopt could not be resumed, so the single-pass
     backend takes it
```

`0x2e` is `iaload`. The IR tier refuses the method because an implicit
`ArrayIndexOutOfBounds`/null trap at pc 4 lives inside a `try` region that has
already committed a side effect, so the deopt has no resumable state. The method
compiles — on the single-pass backend — and the test's subject never runs on the
tier it exists to cover.

**That guard is doing exactly its job.** The header of the test file says the
same thing about a different failure mode: *"On a machine without them this file
provides NO coverage, silently."* Whoever wrote it anticipated a green that
proves nothing and made both shapes loud. This is the second shape.

## What is and is not still covered

The IR stamp itself is **not** unguarded. The file names its own unconditional
twin:

* `jit::ir_lower::tests::the_exception_stub_stamps_one_set_throw_bci_per_distinct_site`
  — pure codegen, no binary and no JDK, red the instant either the stamp or the
  per-bci grouping is removed (both verified by injection rather than by
  trusting a green).

So the regression risk here is not "the stamp broke". It is that the END-TO-END
half — the one that measured 198 927 skipped `finally` bodies before the fix and
0 after — has stopped exercising the path, and would keep reporting FAILED for a
reason unrelated to what it tests.

## What to do

Not "make the test pass". Two honest options, and the choice is the owning
lane's:

1. **Change the probe so it reaches the tier again.** The refusal is specific
   (`iaload` trap inside a side-effecting protected range). A probe body whose
   `try` region has no trapping array access before its side effect would be
   admitted, and the test would resume testing the stamp. The risk is writing a
   probe that is admitted *because* it is no longer the shape that found the
   bug.
2. **Assert the admission instead.** If the IR tier legitimately will not take
   `try`/`finally` bodies of this shape any more, then the end-to-end test is
   asking about a configuration that no longer exists, and it should say so —
   `#[ignore]` with the reason, or an assertion on the refusal itself.

What must NOT happen is relaxing the precondition into a warning. The
precondition is the reason the file is trustworthy.

## Provenance

Measured on Azure host 2, release, `cargo test --release -p cratonvm-vm`:

| ref | result |
|---|---|
| `perf/compiled-ldc-const-cache-20260820` (`e708b1856`) | FAILED, same panic, same admission line |
| merge base `eadd845c4` | FAILED, same panic, same admission line |

Both arms name `pc=4, opcode=0x2e`. Naming the ref matters here: a "red gate on
dev" has been a stale worktree before, so the control is the merge base of the
branch that found it and not a remembered earlier run.

## Related

* `offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-20260804-FIXED.md`
  — the defect this test was written for, and the 198 927 -> 0 measurement.
* `stub-ratchet-was-a-compile-error-and-is-three-over-baseline-20260820.md` —
  the same shape one day earlier: a gate that had stopped reporting, found while
  merging rather than by the gate itself.
