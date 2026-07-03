# G1 parallel evacuator drops a root-referenced object (persistent `forwarding_ptr` vs per-cycle `pointer_map`)

**Status:** 🟢 FIXED (both bugs), soak-verified 2026-07-03. The DOMINANT bug
(persistent-`forwarding_ptr` root-remap, deterministic ~12.5% on the repro)
was fixed first (see "Fix" below). The residual **concurrency race** (~5%,
timing-sensitive) no longer reproduces after the deferred-self-forward-scan
restructure plus the 2026-07-03 change set: **80/80 parallel-evac runs clean
at `-Xmx16m --nojit` 2M iterations** (40× `SteadyChurnLight` — the original
bug note's no-promotion shape — and 40× the heavier `SteadyChurn` recreation;
plus 10/10 serial each; `binarytrees 16 @64m` byte-identical to HotSpot
serial+parallel; gc crate 749/749). See "2026-07-03" below for the FIVE
additional serial-G1 defects that had been masking this validation.
Parallel-evac default-on / the G1 default flip (Step 9/10 of
`docs/feature-designs/concurrent-gc-maturation.md`) is now gated only on the
two REMAINING OPEN follow-ups listed at the end (concurrent-mark liveness
audit; interpreter root-liveness imprecision), not on this race.
The `task_58d60f7a` family turned out to be TWO stacked bugs.

**Scope:** opt-in only (`CRATONVM_G1_PARALLEL_EVAC=1`). The DEFAULT (serial) G1
and Generational are unaffected. Mixed GC is kept on the serial evacuator
because of this.

## Repro (cheap, deterministic-ish)

```
CRATONVM_G1_PARALLEL_EVAC=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  <cv> --nojit -XX:+UseG1GC -Xmx16m -cp scratch/g1par SteadyChurn 2000000
```

~1 in 8 runs throws `Exception in thread "main" java/lang/Object` (correct output
is `2002062093760`). Correct at `-Xmx{24,32,48}m`. **Always correct serially**
(flag off). Reproduces with `--nojit` (no JIT roots). `SteadyChurn` keeps a fixed
4096-node linked list + heavy young churn; at 16m the parallel evacuator exhausts
to-space and self-forwards where serial (which packs into partially-filled
survivors) does not — hence parallel-only and small-heap-only.

`java/lang/Object` is the signature of a **zeroed object header**: the doc in
`types/src/heap_types.rs` notes a well-formed `Object` is `class_id=0, kind=0,
num_slots=0`, so a reused/zeroed region reads back as `java/lang/Object`.

## Root cause (fully diagnosed)

The serial evacuator dedups evacuation via the **per-cycle `pointer_map`**
(`HashMap<old,new>`, fresh each collection). The parallel evacuator instead
dedups via the **persistent `ObjectHeader.forwarding_ptr`** header field (a
lock-free CAS install slot — see `gc/src/g1.rs::SharedEvac::evacuate`). Two
consequences combine into the bug:

1. **`forwarding_ptr` can hold a forward from a PRIOR cycle.** The end-of-cycle
   clear loop only zeroes `key == value` (self-forward) entries; a normally-
   evacuated object that ends up in a region `free_or_keep_cset` KEEPS retains
   `forwarding_ptr = new`. A self-forwarded object in a kept region that is then
   re-collected can leave a stale self-pointer.

2. **A fast-path hit does NOT record the forward in this cycle's `pointer_map`.**
   `evacuate` returns early on `forwarding_ptr != 0` with `(existing, false)` and
   never pushes `(old, existing)` into the worker forward shard.

Now the kicker — **root remapping**. The `roots: &mut [ObjectRef]` slice handed to
the GC is a SNAPSHOT; the GC rewrites it in Phase 1, but the VM applies forwards
to the REAL interpreter frame locals via `update_all_roots`
(`vm/src/memory/gc.rs`), which remaps **through `pointer_map`**. So a root whose
referent was resolved by the parallel fast-path (and therefore is absent from
`pointer_map`) is **never remapped** — the frame local stays pointing at the
from-space object. It survives only as long as that object's persistent
`forwarding_ptr` redirect stays valid. When the redirect is a stale self-pointer,
or the from-space region is freed and reused, the frame local dangles → the
`SteadyChurn` final chain walk reads a zeroed/reused header → `java/lang/Object`.

The deterministic part (the drop) happens every collection; the crash is
probabilistic (needs the freed region to be reused with non-matching bytes),
which is why it's ~1/8.

### Why V7b and the standard checks miss it

- **V7b** (`verify_no_dangling_into_cset`) only scans HEAP objects, never roots,
  and its `!pointer_map.contains_key` test passes for an evacuated target.
- The heap itself is consistent at collection end (all interior refs rewritten by
  Phase 4). Only the **roots** (frame locals) are stuck, and they're fixed up by
  the VM *after* the GC returns — except for the fast-path objects absent from
  `pointer_map`.

## How it was localized

A gated post-collection verifier `dbg_verify_no_unrewritten_forward`
(`CRATONVM_G1_DBG_HEADERS=1`, kept on branch `feat/g1-parallel-evac-race`) checks
every reference in non-CSet regions + roots and reports:

- `OVERLAP` — two from-space objects forwarded to the same dest (NEVER fired →
  ruled out a TLAB allocation race);
- `UN-REWRITTEN` — a non-CSet/root ref to a `pointer_map` KEY (NEVER fired from
  heap holders → the heap is clean);
- `LOST` — a root → a CSet object NOT in `pointer_map` (**fires every collection**
  → the smoking gun).

Phase-1 instrumentation then showed the LOST roots take `evacuate`'s fast path
(`fresh=false`) with `fp_before` pointing at a live copy in another region — i.e.
the root is one-cycle-behind, surviving via the redirect.

## Fixes that DO NOT work (measured — do not retry)

Individually, each of these fails:

| Attempt (alone) | Result | Why it fails alone |
|---|---|---|
| Clear `forwarding_ptr` for all `pointer_map` keys at cycle END | 28/30 bad | removes the redirect that masks the stuck roots; the LOST object is not even a key |
| Clear every CSet object's `forwarding_ptr` at cycle START | 28/30 bad | same, + re-copies the stale from-space object the root is stuck on |
| Record fast-path hits `(old, existing)` into `pointer_map` | 8/30 (~baseline) | `existing` can be a STALE cross-cycle redirect (collected since) → roots remapped to freed memory |

## Fix (landed — dominant bug) — `gc/src/g1.rs`

**Combine the last two** (`evacuate` fast path + the end-of-cycle clear in
`parallel_evacuate`):

1. **Record fast-path hits.** On `forwarding_ptr != 0`, push `(old_ptr, existing)`
   into the worker forward shard before returning — so the forward reaches
   `pointer_map` and the VM can remap a root/ref pointing at `old_ptr`.
2. **Clear ALL keys at cycle end**, not just `key == value` (self-forwards) — so
   no forward persists into the next cycle. With (1)+(2) every forward is recorded
   AND nothing is stale, so a fast-path hit is always a fully-scanned this-cycle
   live copy: the two failure modes of the standalone attempts cancel.

Validated: `SteadyChurn @16m` parallel **38/40** (was ~baseline; the verifier's
LOST check now reports 0 — the root-remap bug is gone); `binarytrees16@64m` and
`PromoteMixed 200000 20000000` serial+parallel **byte-identical to HotSpot**;
734/734 gc tests + new regression `parallel_fast_path_hit_is_recorded_and_root_remapped`.

## Residual — a separate concurrency race (🔴 OPEN), now CHARACTERIZED + LOCALIZED

With the dominant bug fixed, ~2/40 runs still crash. Further investigation
(diagnostic knobs `CRATONVM_G1_WORKERS=N` and `CRATONVM_G1_DBG_ZERO=1`, both gated,
on branch `feat/g1-parallel-evac-race2`) pinned it down:

- **It is a genuine worker-vs-worker concurrency race.** `CRATONVM_G1_WORKERS=1`
  (drain the parallel path serially) is **30/30 clean** — the parallel *logic* is
  correct; only the multi-worker concurrency corrupts.
- **The verifier's LOST check is `0` on every crash** — it is NOT the dominant
  root-remap bug recurring. The new `CRATONVM_G1_DBG_ZERO` scan (walks ALL
  non-Free regions incl. kept ones + roots, flags any reference to an all-zero /
  freed-region header) **fires on every crash**: a KEPT-region self-forwarded
  holder, and sometimes a root, points at an object in a **freed CSet region**.
  So the parallel closure **drops a still-live object** (its CSet region is freed
  by `free_or_keep_cset` because it was neither evacuated nor self-forwarded),
  leaving its referrers dangling.
- **It scales with timing perturbation.** Under both verifiers enabled the crash
  rate jumps to ~19/25 and `DBG-ZERO` reports **80–200** dangling refs — i.e. when
  the bad window is hit, a whole **subgraph** is dropped, not a single object.
  This is consistent with a transient ordering bug, not a steady-state logic gap
  (the gray-queue termination invariant `outstanding ≥ queue.len` holds, and Phase
  1/2 seeding completes before the worker scope).

This is the original `task_58d60f7a` "data race" suspicion, now isolated as a
distinct second bug and localized to the copy ↔ CAS ↔ gray-queue ↔ in-place-scan
handoff for self-forwarded objects under contention. The exact ordering bug needs
a race detector (TSan / loom) or a targeted restructure (e.g. deferring
self-forwarded objects' in-place slot rewrites to the serial Phase-4 pass so they
never race a concurrent copy-read) — not the forwarding-protocol reasoning that
fixed the dominant bug.

## Fix candidate 2026-07-01 - deferred self-forward scans

Implemented the targeted restructure suggested above: freshly self-forwarded
CSet objects are no longer scanned by parallel workers. The parallel workers
record them in a deferred worklist instead of pushing them onto the shared gray
queue, then the driver drains that list serially after all worker threads have
joined. During that serial drain, copied children and further self-forwarded
children are appended to the same serial worklist, so the full transitive closure
is still recorded in `pointer_map` before Phase 4/5.

This removes the suspected copy-read vs in-place-slot-rewrite race: a
self-forwarded holder remains in from-space, and its slots are mutated only after
the parallel copy phase is complete. Focused coverage:

- `parallel_self_forwarded_holders_are_drained_serially`
- `parallel_identity_forwards_seed_serial_drain`
- `parallel_self_forward_clears_forwarding_ptr_across_cycles`
- `parallel_fast_path_hit_is_recorded_and_root_remapped`

Follow-up hardening in this turn derives the serial drain set from the merged
forward shards' identity entries (`old == new`) before Phase 4/5. That makes the
identity forward itself authoritative, so a caller-side missed
`fresh && old == new` side-channel cannot leave a self-forwarded holder unscanned.
It also conservatively serial-scans every object in a CSet region that will be
kept because at least one object in that region self-forwarded. That is a
correctness backstop for evacuation-failure regions: no slot in a kept region is
allowed to retain a stale reference into a CSet region that Phase 5 may free.
Focused coverage: `parallel_identity_forwards_seed_serial_drain` plus the
existing self-forward serial-drain tests.

Status remains fix-candidate/open until the original `SteadyChurn @16m --nojit`
parallel-evac repro is soaked clean enough to retire this known issue. The
original `scratch/g1par/SteadyChurn.java` source was not tracked and was not
present under `C:\craton` on 2026-07-01. A tracked recreation now lives at
`docs/known-issues/repros/g1-parallel-steady-churn/`.

## 2026-07-03 — the "recreation trips serial G1" mystery fully unravelled

The 2026-07-01 caveat ("the recreation also trips serial G1 at longer runs")
turned out to be a STACK of five distinct pre-existing defects plus one repro
flaw, diagnosed and fixed on a Linux probe host (deterministic there: serial
failed at 20k iterations, `Exception in thread "main" unknown`). All landed
with this change set:

1. **Kept-region death spiral (serial + parallel) — FIXED.** `needs_gc`
   triggers at <25% Free, and the Free pool at trigger time IS the young
   evacuation's entire to-space. Once live-young exceeds it, every reached
   object self-forwards, every region holding one is kept wholesale (garbage
   included), and successive pauses monotonically degrade to
   `copied=0, freed=0` — a permanently wedged heap. Fixes:
   `retry_after_evacuation_failure` + `drain_kept_self_forwards` in
   `gc/src/g1.rs` (a same-pause, live-only drain of exactly the
   identity-forwarded objects — see the doc comments for why it must NOT be a
   full re-collection and must never walk a region wholesale) and an adaptive
   `needs_gc` threshold (raised after a failing pause, decays after clean
   ones). Test: `evacuation_failure_retry_drains_kept_garbage`,
   `compose_forward_maps_chases_and_keeps_pause_start_keys`.
2. **`SharedVm::singleton_oom` never remapped — FIXED (vm/src/memory/gc.rs
   step 6c).** The pre-allocated OOME was rooted (kept alive) but its holder
   field was never updated after a move, so any genuine OOM after a relocating
   GC threw a dangling object — the unreadable `Exception in thread "main"
   unknown` that masked everything else.
3. **Adaptive IHOP starvation — FIXED.** `update_ihop` raised the marking
   threshold 5%/collection on fast pauses up to 90% of heap, so concurrent
   marking NEVER started on fast-pause workloads: dead promoted objects
   accumulated in Old un-reclaimed (61k young pauses, zero mixed, true OOM at
   every heap size). The adaptive raise is now capped at the statically
   configured IHOP.
4. **Concurrent-mark cleanup freeing live regions — CONTAINED (open marking
   issue).** `cleanup()` freed any 0-marked Old region; regions filled by
   promotion AFTER the mark snapshot have no marks at all, so cleanup zeroed
   freshly-promoted live objects (TAMS violation). Fixed with a
   `mark_start_snapshot` (per-region `reuse_epoch`/cursor/type; post-snapshot
   bytes count as live). Even TAMS-clean regions were then observed freed
   under live holders — the marker can miss pre-snapshot objects while young
   GCs move their holders mid-cycle (marker-vs-moving-collector liveness, NOT
   audited here). Containment: cleanup no longer frees Old regions in place at
   all — a wholly-dead region has `gc_efficiency == 0.0` and is the first
   thing the next mixed collection evacuates, and mixed reclamation is
   reachability-driven, hence sound. Humongous reclaim keeps the cleanup-time
   path (mixed cannot evacuate spans) with the TAMS guard. The marking
   soundness audit is a follow-up.
5. **Repro flaw — interpreter root liveness imprecision (open VM issue,
   repro fixed).** The recreation's list-setup loop used a construction temp
   (`Node node = new Node(i); tail.next = node; ...`). CratonVM's interpreter
   scans ALL object-typed local slots (no HotSpot-style per-bci oop liveness),
   so the scoped-out temp slot retained `node[4095]` for `main`'s entire
   lifetime — and through `next` chains, EVERY node ever appended: unbounded
   retention that OOMs at any heap size (diagnosed via the new
   `CRATONVM_G1_DBG_ROOTCENSUS=1` per-root reach census: one root, seq=4095,
   reach=67k). Both repro variants now build the list temp-free; the
   interpreter-liveness imprecision itself is a separate, pre-existing VM
   issue affecting all collectors on long-lived frames holding linked
   structures.

Also fixed en route: a parallel drain-phase fixpoint hole (kept regions
discovered during the drain seed further drain rounds —
`parallel_drain_phase_self_forward_kept_region_fully_scanned`), and the drain
no longer walks kept regions wholesale (dead-object resurrection amplified
kept garbage until OOM; stale refs in dead kept objects are safe because a
stale reference can only point at Free/destination regions, never a
CSet-resident live object).

New env-gated diagnostics (all no-ops unless set): `CRATONVM_G1_DBG_REACH=1`
(post-pause BFS from roots reporting any live-reachable ref to a zeroed/wild
header — the tool that pinned every one of the above at its introducing
pause; also enables `[FREED]`, `[RETRY]`, `[PHASES]` and per-pause region
counts on `[GC-STAT]`), `CRATONVM_G1_DBG_ROOTCENSUS=1` (per-root transitive
reach census), `CRATONVM_G1_NO_EVAC_RETRY=1` (bisection kill-switch for the
failure drain).

**Validation (Linux probe host, `--nojit`):** with all of the above,
`SteadyChurn` (heavy recreation) AND the new `SteadyChurnLight` (faithful to
the original's no-promotion shape — see its header comment) print the correct
`2002062093760` at `-Xmx16m` for 2M iterations, serial and parallel;
`binarytrees 16 @64m` is byte-identical to HotSpot serial + parallel; gc crate
749/749 tests.

**Soak results (2026-07-03, Linux probe host, 2M iterations @16m --nojit):**

| suite                         | result      |
|-------------------------------|-------------|
| parallel `SteadyChurnLight`   | 40/40 clean |
| parallel `SteadyChurn` (heavy)| 40/40 clean |
| serial `SteadyChurnLight`     | 10/10 clean |
| serial `SteadyChurn` (heavy)  | 10/10 clean |

The residual race (previously ~2/40 post-dominant-fix) did not reproduce in
80 parallel runs. This known issue's two bugs are considered FIXED.

Remaining OPEN follow-ups (separate issues, tracked in the 2026-07-03 section
above; they gate the parallel-default flip, not this race):

1. Concurrent-mark liveness under moving young collections (cleanup's
   in-place Old-region free stays disabled as containment until audited).
2. Interpreter root liveness imprecision (scoped-out local slots retain their
   last referent for the frame's lifetime — unbounded retention on linked
   structures; both repros are written temp-free to sidestep it).

Parallel evac stays **opt-in/experimental** and mixed stays serial until
those two are resolved. Default G1 (serial) and Generational benefit from the
serial-path fixes above.
