# `TestRandomMapOps` returned `(1810, null)` deterministically

## Status

**FIXED 2026-09-04.** Retired from
`known-issues/h2/bug-testrandommapops-deterministic-1810-null-20260903.md`.

Same defect as
`guarded-inline-native-screen-asked-the-declaring-class-FIXED-20260904.md` —
this was its H2 face. Read that record for the mechanism and the fix; this one
exists for what the H2 page got *wrong*, which is worth keeping.

## What it was

`org.h2.test.store.TestRandomMapOps` failed in 5–23 s, every run, with

```
seed:0 op:1033 java.lang.AssertionError: (1810, null)
```

The message is the test's own `msg` for the cursor range `(from, null)` with
`from = 1810`, and the assertion that fired is the LAST one in that
comparison — `assertFalse(msg, cursor.hasNext())`. So the reading it invites,
"the MVStore cursor yielded more entries than the TreeMap has", is backwards.
The **reference** side was empty: `map.tailMap(1810).entrySet()` iterated zero
entries inside a compiled method, so the loop consumed nothing and the cursor
still had its six.

The oracle was the broken half. `read-which-side-of-a-cross-vm-diff-failed`
is the same lesson from a different harness.

## The bisect found reachability, not introduction

The page bisected `221a383f2..08a1711e5` (92 commits) to **`b4f2e8042`**, a
merge that hand-resolved `jit-api/src/helpers_abi.rs` and `jit-api/src/lib.rs`
where two branches had each appended a field to the JIT helper table. It then
spent its analysis on that resolution — correctly concluding the offsets
agreed and the ABI tests were green, and proposing "test the two parents" as
the next step.

The resolution was fine. The cause is `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE`
having defaulted ON since `f697d618e`, and

```
$ git merge-base --is-ancestor f697d618e 221a383f2 && echo predates
predates
```

**`f697d618e` predates the bisect's own GOOD endpoint.** The defect was latent
in both endpoints; the merge is where the call site started being reached with
the profile evidence a guarded splice needs. A bisect answers "where did this
become observable", and when the introducing change is already behind the good
endpoint that answer is an interaction — which is exactly what the page said
about the other merge it compared itself to, without drawing the conclusion
for its own.

**Worth doing before trusting a bisect that lands on a merge**: check whether
the mechanism you eventually name is inside the range at all. One
`git merge-base --is-ancestor` would have redirected this page's next step.

## Two arms that were unscorable, and the fix for that

The page's ruled-out table rested on runs with no progress signal.
`TestRandomMapOps` prints only `Done pass #N`, and one pass is 100 `testOps`
calls of 3000 ops — 300 000 ops, 11 s on HotSpot and more than an hour in the
CratonVM interpreter. Every interpreting arm (`--nojit`,
`CRATONVM_JIT_DENY=org/h2`) therefore ends "no failure in T seconds, zero
passes", which is not a result.

Measured 2026-09-04: `CRATONVM_JIT_DENY=org/h2` ran **3600 s with zero
passes**. Unscorable, exactly as the sibling box/unbox page warned about its
own `--nojit` arm.

`apps/probes/H2MapOpsProbe.java` is the fix: the first fixed seed only, op-level
progress, and on a mismatch it prints the reference keys, the yielded keys and
the extra entries. It reproduces in **under a second** where the real test
takes 5–23 s, and HotSpot passes it 3000/3000 in 2.1 s. With it, `--nojit`
became answerable in 52 s and answered **clean** — the single highest-value
rung on the ladder, and the one the original page could not reach.

One trap it also exposed: `CRATONVM_JIT_DENY` matches the INTERNAL class name.
`CRATONVM_JIT_DENY=org.h2` matches nothing (the names are `org/h2/...`), so an
arm written with dots is vacuous and reads as an exoneration. `org/h2` bites.
