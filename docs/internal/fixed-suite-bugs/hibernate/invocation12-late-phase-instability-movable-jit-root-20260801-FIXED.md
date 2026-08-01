# FIXED — `DefaultCatalogAndSchemaTest` invocation #12: four different failures, one cause

| | |
|---|---|
| **Status** | ✅ **The relocation hazard below is FIXED** 2026-08-01 — `fix/hib-gcoverhead-halffull-20260731`, commit `404270d2e`. Verified 3 runs for 3 at `132/132 failed=0` against base `32f9db9a2`. |
| **⚠️ Caveat** | That 3-for-3 does **not** hold against the `origin/dev` tip of 2026-08-01, and neither does any other arm's — see [Residual](#residual-the-class-is-intermittently-unstable-in-every-arm). The class is intermittently unstable there whatever the collector does. The hazard fixed here is real and independently argued; it is **not** the whole of that instability. |
| **ID** | `HIB-ANNOTATEDELEMENT-NOCODE.1` (filed under that name when only one of its four faces had been seen) |
| **Found** | 2026-07-31, in the first CratonVM run of the class that ever reached its own end — which only became possible once `HIB-GCOVERHEAD-HALFFULL.1` was fixed. |

## What it looked like

Every failure landed in **`[class-template-invocation:#12]`**, the last of the
twelve `@MethodSource("options")` rows. Nothing else about them was stable —
five runs produced four different faces:

| run | collector state | outcome |
|---|---|---|
| 1 | promotion restored, movable roots unbounded | `found=121 failed=0` — eleven tests never reported |
| 2 | " | `132`, **1 failed** — `AbstractMethodError: java/lang/reflect/AnnotatedElement.getDeclaredAnnotations() has no Code attribute` |
| 3 | " | **SIGSEGV in a JIT frame** at test 123 |
| 4 | promotion OFF (`CRATONVM_NO_SELECTIVE_PROMOTE=1`) | `132`, **4 failed** — `ServiceConfigurationError: BytecodeProviderImpl could not be instantiated` |
| 5 | " | `128`, **5 failed** — same `ServiceConfigurationError` |

Four distinct symptoms — a missing-method dispatch error, a service-loader
instantiation error, silently absent tests, and a segfault — on one binary and
one classpath. That combination really has one explanation: a **stale reference
in a live compiled frame**. An `AnnotatedElement.getDeclaredAnnotations()` call
that resolves to the *interface's* abstract declaration is what a corrupted
receiver looks like from inside dispatch.

## Cause

`sweep_young_non_moving`'s selective promotion protects live objects by **pinning
by value**: every root value landing in young is excluded from evacuation, so a
conservatively-discovered address — real oop or false positive — can only ever be
over-retained, never moved out from under its holder. That is the whole safety
argument for evacuating anything at all while a JIT frame is live.

The pin set had one exclusion: addresses published as **movable** precise-JIT
roots, so a shadow-stack-published oop could be drained rather than pinned.
"Movable" is a *claim* that something will rewrite the frame's copy after the
move — the shadow-stack remap plus the JIT's post-safepoint reload. The exclusion
was applied unconditionally, beneath a comment stating that the movable set is
"empty unless precise relocation is engaged, so byte-identical on the default
path."

That was true when it was written. It stopped being true on **2026-07-31**, when
`conservative_roots::shadow_stack_enabled()` lost its `moving_young_enabled() &&`
term and began defaulting to `true` — so `roots.rs` now publishes every
shadow-stack oop as movable on *every* collection.

Which leaves the fatal case. A cycle whose **moving-young coverage proof failed**
is, by definition, one where a live compiled frame could not show that it
publishes and can rewrite all of its oops. Evacuating one of that frame's roots
there acts on the word of the proof that just failed, and the frame is left
holding a pointer into young memory that gets zeroed and re-served.

## Fix

Condition the exclusion on the proof (`gc/src/gen_heap.rs`):

```rust
let honour_movable = !crate::gc_quiescence::moving_young_coverage_incomplete();
...
if is_y(a) && !(honour_movable && crate::gc_quiescence::is_movable_jit_root(a)) {
    pinned.insert(a);
}
```

Pinning a root for one cycle costs a deferred promotion and nothing else — it is
a *root*, not the interior heap object the drain exists for. `GcPromoteProbe`
promotes a byte-identical 4 616 280 bytes on its first draining cycle with and
without the bound.

It also closes the cross-thread case for free: a cycle can only be "proven" if
`refresh_moving_young_coverage_for_collection()` found no peer in JIT (otherwise
it records `CROSS_THREAD_JIT_PEER`), so the exclusion is now live only when the
initiator is the sole thread in compiled code and every one of its frames proved
coverage — exactly the regime the mechanism was designed for.

## Verification

| arm | promotion | movable bound | result |
|---|---|---|---|
| pre-fix collector (`CRATONVM_NO_SELECTIVE_PROMOTE=1`) ×2 | off | — | `failed=4`, `failed=5` — all inv#12 |
| promotion restored, bound absent ×3 | on | no | `121/0`, `132/1`, SIGSEGV@123 |
| **shipped** ×3 | on | **yes** | **`132/132 failed=0`, 3 for 3** |
| HotSpot control | — | — | `132/132 failed=0` |

Note the first row. With promotion switched off entirely — the collector's
behaviour *before* `HIB-GCOVERHEAD-HALFFULL.1` was fixed — invocation #12 fails
**worse**, 4–5 tests rather than 0–1. So this was not caused by restoring the
drain. It only became *reachable* once the drain was restored, because until then
the process died of `OutOfMemoryError` at ~41 min and never got this far.

Both fixes are load-bearing: without the drain the heap wedges and invocation #12
fails under the resulting pressure; without the bound the drain relocates on a
failed proof.

`a_movable_jit_root_is_pinned_when_the_coverage_proof_failed` (gc unit tests)
asserts both directions — pinned when the proof failed, still evacuated when it
held — so the shadow-stack drain cannot silently become dead code.

## How it was found

`CRATONVM_DBG_NOCODE=1` was armed to catch the receiver at the
`AbstractMethodError` raise site and never fired: the run it was armed for
segfaulted instead. What actually localised the fault was the **differential**,
not the diagnostic — running the same class with `CRATONVM_NO_SELECTIVE_PROMOTE=1`
to ask "does this survive the collector change?", which answered *no, it predates
it*, and pointed the search at what selective promotion is allowed to move rather
than at annotation dispatch.

## Residual: the class is intermittently unstable in every arm

Re-verifying after merging `origin/dev` @ `cc8167f94` showed the 3-for-3 above
does not survive that tip — and, importantly, **nothing else does either**:

| arm | runs | outcome |
|---|---|---|
| pure `origin/dev` (old gate, so no promotion) | 5 | 4 clean; 1 × `ServiceConfigurationError` at inv#5, run aborted at 50 tests |
| merged + `CRATONVM_NO_SELECTIVE_PROMOTE=1` | 3 | 1 clean; 1 × **6** failures at 54 tests; 1 × 1 failure at 11 tests |
| merged + the fixes on this branch | 4 | 2 × clean `132/132`; 1 × `132/132 failed=0` then SIGSEGV in teardown; 1 × early `capacity overflow` panic |
| base `32f9db9a2` + the fixes on this branch | 3 | 3 × clean `132/132` |

Read the rows against each other, not individually:

* The instability is **in every arm**, including collectors that never promote at
  all. It is therefore not caused by restoring the drain, and not caused by the
  movable-root bound.
* The promotion-ON arm is the only one where **every run that completed its plan
  reported all 132 tests passing**. Both promotion-OFF arms produced real test
  failures and truncated runs. On outcomes, the fixes help.
* The two promotion-ON crashes are in the *tail*: one after all 132 tests had
  passed (during teardown, before `testPlanExecutionFinished`), one early and
  never reproduced in 6 further runs of the same binary.

So: the specific hazard this document describes is fixed and the argument for
fixing it stands on its own (relocating a root on the word of a proof that just
failed is wrong regardless). But the class-level "it passes now" claim is
retracted for the `dev` tip. The remaining instability is a **separate, older,
still-open** fault whose faces include `ServiceConfigurationError` on
`BytecodeProviderImpl`, truncated test counts, and teardown segfaults, and which
predates this branch.

Do not use this class as a regression gate for any of it — see the warning in
`../../../known-issues/hibernate/README.md`. Use `probes/GcPromoteProbe.java`
and the gc unit tests.

## Lesson

A comment asserting an invariant ("this set is empty on the default path") is a
claim about *other* code, and nothing rechecks it when that other code changes.
The default flip that invalidated this one landed the same week, in a different
crate, for an unrelated reason, and left no failing test — because the hazard it
armed was dormant behind a second bug (promotion switched off) that was itself
masked by a third (the process OOMing before it got far enough to matter).

## Related

- [`gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md`](gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md)
  — the OOM whose fix made this class reach its own end for the first time.
- [`../../../known-issues/hibernate/moving-young-inert-under-jit-throughput-tax-20260730.md`](../../../known-issues/hibernate/moving-young-inert-under-jit-throughput-tax-20260730.md)
  — still open, still a throughput tax: the class takes ~35–50 min where HotSpot
  takes 120 s.
