# `TestDefaultInstanceManager.testClassUnloading` — third occurrence (FIXED)

**Status:** FIXED and retired on 2026-08-01. Retires
`docs/known-issues/tomcat/defaultinstancemanager-third-recurrence-OPEN.md`.
Third and (so far) last entry in the chain that starts at
[defaultinstancemanager-classunloading-count-mismatch-FIXED.md](defaultinstancemanager-classunloading-count-mismatch-FIXED.md)
(2026-07-14) and continues through
[defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md](defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md)
(2026-07-27).

## Symptom

```
java.lang.AssertionError: expected:<8> but was:<9>
	at org.apache.catalina.core.TestDefaultInstanceManager.testClassUnloading(TestDefaultInstanceManager.java:66)
```

Identical to both prior writeups, and deterministic — 2/2 here, matching the
OPEN doc's 2/2.

## Root cause — the 07-27 fix was PRESENT and INERT

The OPEN doc verified that the fix code was in the tree and compiled into the
binary under test (`mirror_pin_deferrable` in `gc/src/vm_heap.rs`, referenced
from `vm/src/memory/roots.rs`). That check was correct and it proved nothing:
the code ran and its guard was hard-wired `false`.

`VmHeap::mirror_pin_deferrable` leaves a still-YOUNG class mirror out of the
unconditional root set only when
`gc_quiescence::young_marker_follows_side_tables()` promises that some marker
running this cycle will actually follow `mirror_pin`. That predicate read:

```rust
!dbg_force_moving
    && !moving_young_enabled()
    && (is_active() || unregistered_jit_frame_on_stack() || major_gc_requested())
```

i.e. it factored `!moving_young_enabled()` across all three disjuncts. It
belongs to only one of them. The collector decision it is written to mirror,
`GenerationalHeap::collect_garbage_inner`'s `divert_non_moving`, is:

```rust
(has_conservative_roots && !moving_young)     // the guard belongs HERE
    || honor_promotion_oom_risk
    || divert_for_incomplete_moving_coverage
    || explicit_full_gc                       // ...and NOT here
```

`explicit_full_gc` is `major_gc_requested()` — an explicit `System.gc()`, which
is exactly what this test forces. It diverts the cycle to the non-moving young
marker **on its own**, with no moving-young condition attached.

That mismatch was invisible while `DEFAULT_MOVING_YOUNG` was `false`, because
then `!moving_young_enabled()` was true by default and the term cost nothing.
Commit `67de5400a` ("fix GC corruption in liquibase refreshes", 2026-07-28)
flipped it to `true` — **one day after** the fix landed (`07f9d0c52`, 07-27,
verified 3/3 that same day). From that commit onward the predicate returned
`false` unconditionally on the shipped default. Every young mirror was rooted
directly again; a mirror's `classLoader` field is a real heap edge, so the
JasperLoader and the whole JDT compiler graph behind it stayed reachable, and
`WeakReference<Class>` never cleared.

This also answers the question the OPEN doc raised and could not settle. It
noted the new determinism was "itself informative" and expected occasional
PASSes if this were the same timing-sensitive defect. Correct instinct, wrong
conclusion: it is the same defect, but it is no longer timing-sensitive. The
07-14/07-27 writeups describe a *promotion-timing accident* (does the mirror
happen to be old-gen when `System.gc()` runs). The 07-28 flip replaced that
accident with a structural certainty — the deferral cannot engage at all — so
the failure became deterministic.

Defect 2 of the 07-27 pair (a parked thread's JIT memo caches pinning its last
working set, fixed in `deposit_root_snapshot`) was unaffected and still holds.
This was defect 1 alone, re-opened.

### The flip was nominal for two days, which sharpens the point

`docs/internal/default-moving-young-enabled-20260730.md` records that from
`67de5400a` until 07-30 the constant was `true` while **no cycle actually
moved** — `cycles=0 coverage_fallbacks=66` on `BinTreesClassic 18`; three
separate defects kept diverting every collection to the non-moving sweep.

So during that window the young marker really was `mark_young_precise_object`,
really was following `mirror_pin` — and the root gatherer had already stopped
believing it would. The predicate was not merely pessimistic about a future
moving cycle; it was wrong about the collector running right then. "Is the flag
on?" and "will this cycle relocate?" are different questions, and a predicate
that asks the first while meaning the second is wrong even while the flag is
telling the truth.

## Fix

`gc/src/gc_quiescence.rs` — `young_marker_follows_side_tables()` is now written
arm by arm against `divert_non_moving` instead of factoring a common guard:

- `CRATONVM_DBG_FORCE_MOVING` vetoes everything (it is the only switch that can
  carry a cycle *past* the `divert_non_moving` branch).
- `major_gc_requested()` ⇒ `true`, independent of moving-young.
- otherwise `!moving_young_enabled() && (is_active() || unregistered_jit_frame_on_stack())`.

Still errs strictly toward `false`, as the original design intended: the two
per-cycle verdicts (`honor_promotion_oom_risk`,
`divert_for_incomplete_moving_coverage`) are not decided when the root gatherer
asks, so they remain ignored. A false negative costs one extra conservative
root; a false positive would drop a live one.

No behaviour change for a MOVING young cycle: a young mirror still gets an
unconditional root there, because the Cheney closure seeds strictly from the
direct root set and does not follow `mirror_pin`. Class unloading for a
not-yet-promoted mirror therefore still requires a non-moving cycle — which is
what `System.gc()` always takes, and is consistent with HotSpot only unloading
classes on a full GC.

## Verification

Local Windows fixture (`apps/tomcat`, `run-one.ps1`, suite env
`CRATONVM_REAL_NET_SOCKETS/REAL_AQS/DISABLE_DEFAULT_WATCHDOG/ROOTSNAP_CACHE=1`,
`-Xmx2g`, real JDK). Baseline = `dev` tip `cfa3db80f`
(`cratonvm-definstmgr-base-20260801.exe`); fixed =
`cratonvm-definstmgr-fix-20260801.exe`.

| binary | env | result | wall |
|---|---|---|---|
| baseline | default | **FAIL** `expected:<8> but was:<9>` | 19.6s |
| baseline | default (repeat) | **FAIL** same | 17.3s |
| baseline | `CRATONVM_NO_MOVING_YOUNG=1` | OK (1 test) | 16.9s |
| baseline | `CRATONVM_NO_MOVING_YOUNG=1` (repeat) | OK (1 test) | 13.1s |
| fixed | default | OK (1 test) | 19.0s |
| fixed | default | OK (1 test) | 19.1s |
| fixed | default | OK (1 test) | 19.4s |
| fixed | `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER=1` | **FAIL** `expected:<8> but was:<9>` | 17.4s |
| fixed | `--nojit` | OK (1 test) | 21.8s |

The baseline `CRATONVM_NO_MOVING_YOUNG=1` row is the differential that
identifies the cause: same binary, same fixture, one flag, PASS/FAIL flips. The
fixed `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER=1` row is the mutation check proving
the repaired lever is the one doing the work rather than something incidental
having shifted.

Unit: `cargo test -p cratonvm-gc --lib` — 952 passed, 0 failed.

## Two new unit tests, and why they assert BOTH gate values

`explicit_full_gc_follows_side_tables_on_either_moving_young_gate` and
`conservative_jit_roots_follow_side_tables_only_without_moving_young`
(`gc/src/gc_quiescence.rs`). The predicate had **no** test before this; that is
the whole reason a one-line default flip could disarm it in silence.

The first test loops over both published values of the moving-young gate on
purpose. Mutation-checked by restoring the pre-fix predicate: it passes the
`moving_young=false` iteration and fails the `true` one. A one-sided
assertion — the natural thing to write — would have passed both before the flip
and after it, and caught nothing.

## Lessons

1. **"The fix code is present and compiled in" is not evidence the fix is
   running.** The OPEN doc's check #3 verified presence and stopped there. What
   distinguishes a present-and-working fix from a present-and-inert one is a
   differential on the lever itself (`CRATONVM_NO_MOVING_YOUNG` here), not a
   `grep` of the source tree.
2. **A guard that reads a default is a guard that a default flip can delete.**
   The flip in `67de5400a` was a one-line change in `types/src/flags.rs`, made
   for an unrelated Liquibase corruption fix, with no reason for its author to
   look at Tomcat class unloading. Nothing failed. The predicate simply stopped
   being true.
3. **When you write a predicate that "mirrors" another decision, mirror it term
   by term.** The bug was a distributed `&&` — algebraically tempting, and wrong
   because one disjunct of `divert_non_moving` genuinely has no moving-young
   condition. The sibling predicate
   `vm::memory::roots::conditional_loader_metadata` asks the same question, was
   written without the term, and never broke.
4. **A determinism change is a cause change.** Two prior writeups documented
   this failure as promotion-timing-dependent. When it came back deterministic,
   that was the signal that something had made the deferral structurally
   impossible rather than merely unlucky.

## Audit

All 8 `moving_young_enabled()` reads in `gc/`, `vm/` and `jit/` were examined,
not grepped past. Seven are correct:

| site | verdict |
|---|---|
| `conservative_roots.rs` ×4 (475, 1613, 1897, 2215) | early-outs of moving-young-specific machinery; return the safe value when the flag is off |
| `licm.rs::self_call_moving_proof_enabled` | genuinely moving-specific (elides a self-call spill only when relocation is in play) |
| `x64.rs::moving_young_safepoint_coverage_complete` | *is* the per-frame relocation proof |
| `x64.rs:4104` `precise_maps && !moving_young_enabled()` | **looked like rot and is not** — see below |
| `gc_quiescence::young_marker_follows_side_tables` | **the defect fixed here** |

The reload gate deserved the second look. `emit_post_safepoint_reload` exists to
repair register invisibility after objects move, and disabling it exactly when
the moving collector is on reads as inverted — the more so because
`invoke.rs:15302` documents a real corruption caused by that reload skipping an
oop parameter "so a moving young GC during that call rewrote the canonical frame
slot but left the register stale". It is nevertheless correct: under
moving-young `collect_live_oop_homes` publishes every live oop *including
`ShadowHome::Reg` register homes* as rewritable roots, the GC rewrites those
shadow entries, and `emit_shadow_reload` (`x64.rs:3892`) loads them back into
the registers. Two mechanisms for one job, correctly switched — the frame-slot
reload for the precise-maps path, the shadow reload for the moving path.

The mirror image of this rot was found on 2026-07-31 at a neighbouring site:
three root-visibility mechanisms live *only* because the default is on, all
three withdrawn together by `CRATONVM_NO_MOVING_YOUNG=1`
(`known-issues/jit/jit-no-moving-young-opt-out-unpublishes-roots.md`). Two
instances in two days, opposite directions, one root shape: a fix keyed on this
flag for correlation rather than for meaning. The checklist that follows from
that now lives on `DEFAULT_MOVING_YOUNG`'s own doc comment and on the
`empty_source_matches_all_documented_defaults` pin assertion, so the next person
to edit the constant meets it before the flip rather than three days after.

## Reproduction

```powershell
apps\tomcat-suite-runner\run-one.ps1 `
  -Class org.apache.catalina.core.TestDefaultInstanceManager `
  -Exe <cratonvm.exe> -TimeoutSec 900
```

`run-one.ps1` sets the four suite variables and `Push-Location`s to
`apps\tomcat` itself. Running the raw command line from the repo root instead
produces an unrelated `FileNotFoundException: conf\logging.properties` that
looks like a different bug — the CWD trap the OPEN doc warned about, and the
reason to prefer the runner over a hand-assembled invocation.
