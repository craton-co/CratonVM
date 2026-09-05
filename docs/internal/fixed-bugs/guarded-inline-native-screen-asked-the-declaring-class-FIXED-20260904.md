# The guarded-inline native screen asked the declaring class, not the receiver's

## Status

**FIXED 2026-09-04.** Retired from
`known-issues/jit/bug-compiled-treemap-submap-iteration-empty-20260903.md`.
The default was flipped OFF in `70bf16e19`, which removed the wrong answer and
named the receiver handling as the thing still to fix. This is that.

## What happened

A compiled `for (e : treeMap.tailMap(k).entrySet())` iterated **zero** entries
while `entrySet().size()` on the same object answered 6 — deterministically
from about iteration 500, on `probes/TreeTailIterProbe.java`. The data was
intact; re-iterating the same view object in another method gave 6.

This is H2 `org.h2.test.store.TestRandomMapOps` `seed:0 op:1033`, which is
also `h2/bug-testrandommapops-deterministic-1810-null-…` (retired beside this
one), and it blocked
`known-issues/jit/bug-box-unbox-intrinsic-segv-under-relocation-20260902.md`
by killing the workload well before that page's window opened.

## Root cause

`CRATONVM_DBG_JITC=1` with the feature on names **exactly one splice in the
whole probe**:

```
[cratonvm-jitc] inline-plan pc=22 java/util/Iterator.hasNext()Z:
    Monomorphic { guard_class_id: 228 }
```

Class 228 is `java/util/TreeMap$EntryIterator` — which **does** have
`native_al_itr_has_next` registered on it, by the `VALUES_ITR_CARRIERS` loop in
`native-collections/src/lib.rs`. The splice should have been refused.

It was not, because `resolve_inline_site_from` screened the class that
**declares** the selected method, and `hasNext` is declared one class up, on
`java/util/TreeMap$PrivateEntryIterator`, which carries no native. The screen
cleared, and the splice ran the real JDK body —

```java
public final boolean hasNext() { return next != null; }
```

— over a `next` field a natively-managed iterator never populates. `false`, on
every call, from the first compiled one.

**The rule the two existing screens state is right; their subject was wrong.**
One asks about the constant-pool class, the other about the declaring class. A
guarded virtual site has neither: it starts selection at the *runtime
receiver*, `find_method_recursive` returns the first concrete body it meets,
and the screen then looks at that body's declaring class — one or more classes
*above* the one carrying the native. A native is registered on the class the
receiver actually has.

## The fix

Screen the whole receiver-to-declaring chain, which is the question dispatch
itself asks: not "does the class that wrote this method have a native" but
"does any class this receiver **is** have one". Bounded by the declaring class
— past it the body is not the one being spliced — and short in practice.

## Verification

`probes/TreeTailIterProbe.java`, 3000 iterations, release build, same host:

| arm | result |
|---|---|
| `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=1`, before | `FIRST tail divergence iter=508` / `505` — 2/2 |
| `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=1`, after | `badTail=0 badHead=0` — **3/3** |
| default (flag off), after | `badTail=0 badHead=0` — 3/3 |

### The standing check, corrected

`regression-suite/src/RJitTreeSubMapIter.java` was named here as the standing
check. It is the standing check for the DEFAULT, and it cannot see this defect:
`run.sh` sets no environment, so every suite vector runs with
`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` off, and with it off no guarded site is
ever planned. It pins the flip in `70bf16e19`; it would have gone green against
the unfixed screen on any day of the last month.

`vm/tests/jit_guarded_inline_native_shadow.rs` (added 2026-09-04) is the arm
that sets the flag. It spawns the real binary -- the flag has to be set before
the process starts to be seen by the memoising `env_cache` reader, and the
in-process `Vm::new(VmConfig::new())` that `pgo02_guarded_virtual_inline.rs`
drives cannot build these views at all (`entrySet().iterator()` answers `null`
there, before any of this is reached). Its fixture is the probe with the
`EMPTY-ITER` diagnostic, and it was verified to FAIL on the pre-fix binary and
PASS after, not assumed to:

| binary | flag | fixture |
|---|---|---|
| pre-fix | ON | `FAIL ... badTail=2495 badHead=0 firstBad=504` |
| fixed | ON | `PASS` |
| fixed | shipped default | `PASS` |
| HotSpot | -- | `PASS` |

`vm/tests/pgo02_guarded_virtual_inline.rs` still passes with its
`speculative_sites > 0` assertions, so the new screen did not simply switch
guarded inlining off.

### H2 discriminates on ONE failure mode, and H2 is not green either way

`org.h2.test.store.TestRandomMapOps --Xmx 256m`, ABBA-interleaved on host `vm1`
at load 3-5, `timeout 420`, pre-fix and fixed binaries built from the same tree.
Read the failure KIND, not the wall clock: this test seeds its first pass with
`0` and then draws `r.nextLong()` for the next 99, so no two runs after that
first pass are the same program.

| arm | binary | flag | n | outcomes |
|---|---|---|---|---|
| A | fixed | ON | 4 | 3x SIGSEGV (85, 91, 276 s), 1x `AssertionError: Expected: 98 actual: 78` (295 s) |
| B | pre-fix | ON | 4 | **4x `NullPointerException`** (102, 133, 158, 366 s) |
| C | fixed | shipped default | 3 | 3x SIGSEGV (84, 85, 400 s) |
| D | pre-fix | shipped default | 2 | 2x TIMEOUT at 420 s |

The signal is the `NullPointerException` out of `MVStore`'s store-header read,
with an iterator `hasNext` on the walk: **4 of 4 on pre-fix-with-the-flag, 0 of
9 everywhere else.** That failure mode belongs to this defect and it is gone.

Everything else on that table is the workload's existing state on this host, and
C against D is the proof of how little one row is worth: with the flag OFF this
screen cannot execute at all, so those two arms are the same code, and they
still disagree (SEGV vs TIMEOUT). Do not read A's rows as a regression, and do
not read this table as "H2 passes".

## What is NOT changed, and why

**The default stays OFF.** `70bf16e19` restored it to what three separate doc
comments in `jit/src/lib.rs` say it is — "default-off, unsoaked" — after it
had defaulted ON since `f697d618e`, a commit whose own subject reads
"checkpoint, no codegen yet". This change removes the blocker that commit named
for turning it back on; it does not do the soak. Whoever takes that up has the
probe and the regression vector as the check, and should expect the same class
of defect anywhere a natively-shadowed method is declared on a supertype.

## The lesson worth keeping

**A screen inherits the question its subject can answer.** "Does this class
carry a native" is a precise question with a different answer at every level of
a hierarchy, and the level that matters is the one dispatch would pick. Two
call sites asked it of the constant-pool class and the declaring class because
that is what those paths *had*; the third path had a receiver and reused a
screen written for callers that did not.
