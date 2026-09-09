# The non-moving sweep reclaims a live `TypeDescription$Generic` — and it now reproduces in two minutes on Linux

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-08, **re-opened with a Linux reproducer the same day**. Two adjacent root-coverage gaps found and FIXED (below); neither is this one. |
| **Scope** | `--XX:UseGc Generational`, JIT on, `CRATONVM_DBG_GC_STRESS=4194304`. Passes without the stress interval. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests` — **26/27, ~110 s**, Linux x86-64, `dev` @ `56f5a2fd3` |
| **Victim** | `net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType$ForLoadedType` |
| **Collector** | the **NON-MOVING** young sweep. `moving=2 non_moving=6274`, reason `nonmoving-conservative-jit-roots`. |

## What changed since the first revision

The page was filed as a Windows-only, "not root-caused" report with a lead. Three
things are different now:

1. **It reproduces on the shared Linux box**, at this page's own configuration,
   in about two minutes — and an **unmodified `dev` binary reproduces it**, so
   it is not anyone's in-flight work. It did NOT reproduce on `dev` @
   `a58ebd5ca` earlier the same day (4 young cycles, all MOVING); the newer tip
   keeps a JIT frame live at collection time for this workload, which is what
   selects the sweep, and everything follows from that.
2. **The Mockito failure is not a second, independent thing.** The page's
   second revision split them; that was wrong, or at least it is one event
   here. The full chain is:

   ```text
   MockitoException: Mockito cannot mock this class: interface java.lang.annotation.Annotation
     …
     Caused by: java.lang.IllegalArgumentException: Could not create type
     …
     Caused by: java.lang.NoSuchMethodError:
         'net.bytebuddy.description.type.TypeDescription$Generic java.lang.Object.asGenericType()'
       at java.util.AbstractList$Itr.next(AbstractList.java:373)
       at net.bytebuddy.utility.CompoundList.of(CompoundList.java:83)
   ```

   `java.lang.Object.asGenericType()` is the all-zero header of a reclaimed
   span resolving to `ClassId(0)`. The "third run with `guard=0`" that
   motivated the split has a duller explanation than a second defect: the
   free-list and freed-span rings are bounded, and these runs do 5 000–15 000
   sweeps.
3. **The page's own experiment #2 has been run**, and it answers.

## The experiment table — one binary, one config, one lever at a time

`CRATONVM_DBG_GC_STRESS=4194304`, Generational, JIT on, 27 tests.

| lever | what it changes | cycles | result |
|---|---|---|---|
| *(none)* | | `moving=2 non_moving=6274` | **FAIL 26/27** |
| *(none)*, unmodified `dev` binary | control | `moving=2 non_moving=6274` | **FAIL 26/27** — pre-existing |
| `CRATONVM_DBG_FORCE_MOVING=1` | moving young collector | `moving=4 non_moving=0` | PASS 27/27 |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` | conservatively scan the WHOLE native stack, on every root-snapshot publisher | `non_moving=15007` | **PASS 27/27** |
| `CRATONVM_NO_PRECISE_JIT_MAPS=1` | every chain entry conservative → the chain band at its widest | `non_moving=5168` | FAIL 26/27 |
| `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` | the A5 probe's residue filter's veto removed | `non_moving=5184` | FAIL 26/27 |
| `CRATONVM_JIT_ABOVE_CHAIN_SCAN=1` | the band `[max(entry_sp), stack_high)`, walked once per collection | `non_moving=4978`, 3 056 492 roots, 1.0 GB read | FAIL 26/27 |
| `CRATONVM_JIT_ABOVE_CHAIN_SCAN=1` **+** `CRATONVM_NO_PRECISE_JIT_MAPS=1` | both bands at once | `non_moving=5213` | FAIL 26/27 |

Read it as three facts:

* **It is the non-moving sweep.** Forcing the moving collector cures it, and
  that is not a vacuous control here (the moving collector genuinely runs; the
  page's warning about `--nojit` does not apply, because JIT stays on).
* **The missed root is reachable by a conservative native-stack scan.** The
  fullstack diagnostic differs from the normal path in exactly that and cures
  it, over 2.4x as many collections as the failing run.
* **It is NOT the range.** Every widening of WHICH bytes get scanned —
  above the chain, the whole chain, the A5 band — leaves the failure exactly
  where it was, and so does their union. What is left of the fullstack
  diagnostic's difference is the **PATH**: it also scans from
  `update_root_snapshot`, on every object-returning native call, and from the
  blocked-deposit path. The collection's own root pass is not enough.

## Next — and it is a narrow question now

**Why does a scan that runs at every native call find a root that the same
scan, run at collection time on the same thread, does not?** Three candidates,
in the order they are cheap to test:

1. **Another thread.** `update_root_snapshot` publishes into the OWNING
   thread's snapshot; the collection's pass scans only the initiator. If a peer
   (Mockito's inline mock maker self-attaches an agent; there are also the JDK
   `Common-Cleaner` and `Reference Handler` threads) holds the reference in a
   Rust frame, only its own snapshot can publish it. Test: `-Parallel 1` is
   already the case, so count the threads —
   `CRATONVM_DBG_JIT_ROOTSCAN=1` prints `xt_cov=(accepted=… refused=…
   deposits=…)` per collection; a non-zero `deposits` says peers are
   depositing at all.
2. **A window the collection pass cannot see.** The reference exists in a Rust
   frame at native-call time and has been popped by the time the collection's
   pass runs, while the object is still live through it — i.e. the object is
   reachable at the collection point only via memory the snapshot captured
   earlier. That would make the snapshot a *cache* the pass is missing, not a
   peer-visibility mechanism.
3. **`publish_pinned_jit_roots` replace semantics.** Both paths publish this
   thread's pin set with REPLACE semantics. If the collection's pass publishes
   a smaller set than the last snapshot did, it *withdraws* pins the snapshot
   had taken.

`CRATONVM_JIT_ABOVE_CHAIN_ALL_PATHS=1` (with `CRATONVM_JIT_ABOVE_CHAIN_SCAN=1`)
is in the tree to separate (1)+(2) from (3) without a rebuild: it puts the band
on the snapshot publishers too. If that alone cures the failure, the answer is
in the path and the range work above is closed out; if it does not, the
fullstack diagnostic's remaining difference is the LOW bound (`scanner_sp`
rather than the chain top) on those paths, and
`CRATONVM_JIT_ABOVE_CHAIN_FROM_SP=1` says so.

## Two root-coverage gaps found on the way, and FIXED

Neither is this failure — `[GC] a5_frame_pass: cycles=0` on every run in the
table, i.e. the first repair does not even engage on this defect's cycles —
but both are real, and one of them is the lead this page filed.

### 1. The A5 cycle ran the sweep without the pass that makes it safe

The collector takes the non-moving sweep for either of two reasons:

```rust
let has_conservative_roots = gc_quiescence::is_active()
    || gc_quiescence::unregistered_jit_frame_on_stack();   // A5
```

`roots::conservative_locals_enabled` — which suppresses the per-bci
local-liveness filter and runs the tag-independent frame probes that recover a
reference from a slot whose `CompactValue` object tag was lost — keyed on the
FIRST reason only. On an A5-only cycle the sweep therefore froze on
`GC_FLAG_MARKED` with the pass that exists to widen its root set switched off.

Widening that predicate in place is wrong, and this page said why: `collect_roots`
clears the A5 flag at the top of the pass and only step 14's JIT scan re-sets
it, so reading it at step 1 answers about the wrong cycle. **Step 14a5** runs
the same pass immediately after that scan instead, where the flag is
authoritative and the root vector is still being built. Sound by a stricter
argument than step 1's: the scan that sets the flag also records
`UNREGISTERED_JIT_FRAME` as an incomplete-coverage reason, which
`collect_garbage_inner` honours through `divert_for_incomplete_moving_coverage`
— overriding even `CRATONVM_DBG_FORCE_MOVING` — so on every cycle the pass
fires the collection provably does not relocate.

Engagement, printed under `CRATONVM_GC_STATS=1` as `[GC] a5_frame_pass:`, on a
run with the A5 accept forced (`CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1`,
`GC_STRESS=524288`) against `dev` @ `a58ebd5ca`: **8384 of 8539 non-moving
cycles ran the repair, 5 248 997 extra roots, 27/27, no wall-clock regression**
(70.7 s vs 82.6 s pre-fix). The 155-cycle shortfall is cycles that were
non-moving for another reason or that already had `is_active()`; the pass
declines both. Unforced, `cycles=0` — the negative control.

The same command on the newer tip (`56f5a2fd3`) reports `cycles=11`, and that
is the point rather than a contradiction: there, `is_active()` is true for
almost every collection, so step 1's probe already covers them and the repair
correctly stands down. The A5-only cycle is the rare one; it is also the one
that had nothing.

Two unit tests in `vm/src/memory/roots.rs` pin both halves: that the ordinary
scan must NOT root a long-tagged slot (or the pass is not what keeps the object
alive), and that the pass does; and the engagement predicate's truth table,
including that it stays off when the cycle may move.

The peer deposit paths keep the `is_active()` gate deliberately: they run
before the cycle picks a collector and cannot make that argument.

### 2. The third predicate was dead

This page listed three sites asking "is the non-moving sweep the collector that
will run?" and said they disagree. Two of them did. The third could not have:

| site | as written | as evaluated |
|---|---|---|
| `gen_heap.rs` `has_conservative_roots` | `is_active() \|\| unregistered_jit_frame_on_stack()` | as written — it runs after the root scan |
| `roots.rs` `conditional_loader_metadata` | `is_active() \|\| unregistered_jit_frame_on_stack() \|\| major_gc_requested()` | **`is_active() \|\| major_gc_requested()`** |
| `roots.rs` `conservative_locals_enabled` | `is_active()` | as written |

`collect_roots` calls `clear_unregistered_jit_frame_on_stack()` two statements
above `conditional_loader_metadata`. `gc_quiescence::young_marker_follows_side_tables`'
own doc had recorded both halves of this — that the flag "is always `false` at
the mirror call site", and that `conditional_loader_metadata` "correctly never
had the term" — while the term sat in the tree. It is removed, and that comment
is true again; `false` there is the safe direction (it keeps the conservative
unconditional mirror rooting).

## Three instruments were lying, and are fixed

1. **The off-grid sweep-anchor counter reported the walk's own start.** This
   page noted two OTHER classes in the same suite reporting `off_grid=1
   anchors=2` while PASSING, and that the counter "is not" the exactly-zero its
   own message claims. It was a false positive, and the shape proves it: a
   two-entry list is `[0, used]`, and `used` is unreachable from inside a
   `cursor < used` loop, so the only anchor it can have counted is offset 0 —
   which the anchor builder *deliberately exempts* from its own free-block
   filter ("it is the walk's start, not a split point"). The probe ran after
   `skip_free_blocks`, so a free block at the front of from-space made the walk
   resync past 0 and `continue`, and the next iteration counted it. Now
   `gen_heap::AnchorGridProbe`, which consumes the exempt walk start up front
   and treats a resync over a KNOWN free block as accounted for rather than
   skipped — while still counting an anchor passed by an object stride. Four
   unit tests pin exactly that split.
2. **The local-liveness ledger had no age.** `liveness_filtered_at` is keyed by
   address with no invalidation on re-serve, and the guard printed its hit as a
   broken contract ("the filter guarantees such a slot is never read again; it
   was"). Entries now carry the collection they were made on and the report
   prints `collections_since`. First reading after the change: **44** — the
   frame it had been naming was about a different object, and
   `CRATONVM_NO_LOCAL_LIVENESS=1` reproduces the failure it was being blamed
   for.
3. **An orphaned `#[cfg]`.** `dbg_fullstack_scan`'s doc comment and its
   `#[cfg(any(windows, linux))]` had been left ~200 lines from the function
   when it moved, and had attached themselves to `UNREG_MEMO_SUPPRESSED` —
   cfg-gating a counter `vm-cli` reads unconditionally.

Also corrected: `conservative_locals_enabled` claimed its `real_forkjoinpool`
gate was "OFF — the default for the entire app gauntlet", so the probe was
"byte-identical to baseline". That flag defaults **on** (the synthetic pool is
the opt-in), and any blast-radius argument built on that sentence was built on
a default that no longer exists.

## What was ruled out

* **Not a missed write barrier.** 16 860 `[rset-verify]` reports, `missing=0`
  in every one.
* **Not the off-grid sweep anchor.** That counter's non-zero readings were the
  instrument, not the sweep — see above. This class reports `off_grid=0`
  either way.
* **Not the per-bci local-liveness filter.** `CRATONVM_NO_LOCAL_LIVENESS=1`
  reproduces.
* **Not the parallel sweep.** `CRATONVM_DBG_SWEEP_ZERO=1` switches the young
  collector to the sequential walk; it reproduces.
* **Not the default configuration.** Without the stress interval this class
  passes.
* **Not the A5 repair's territory** — `a5_frame_pass: cycles=0` throughout.

## Repro

```bash
CRATONVM_DBG_GC_STRESS=4194304 \
CRATONVM_GC_STATS=1 \
pwsh -NoProfile -Command "& '<repo>/apps/spring-boot-suite-runner/run-spring-boot-suite.ps1' \
  -Exe <cratonvm> -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
  -ClassList <core/spring-boot BindableTests> -Parallel 1 -TimeoutSec 1800 \
  -CratonArgs @('--XX:UseGc','Generational')"
```

`CRATONVM_GC_VERIFY_RSET` is NOT needed and costs a full old-generation walk per
collection. **Do not use `--nojit` as a control** — with no JIT frame the
collector takes the moving path, which frees nothing into a free list, so the
free-list guard's second condition can never hold and the arm reads clean
whatever the truth is. `CRATONVM_DBG_FORCE_MOVING=1` is the honest version of
that control, and `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` is the one that holds
the collector on the sweep without touching whether JIT frames exist.

## A different defect, found here, filed separately

`bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md` — the
same class under harsher stress (`<= 262144`) fails deterministically after
exactly 1203 **moving** cycles with an operand-stack slot the frame remap did
not reach. Different collector, different failure; folding it in here would
have buried a repro that is deterministic to the cycle.
