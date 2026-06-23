# G1 parallel evacuator drops a root-referenced object (persistent `forwarding_ptr` vs per-cycle `pointer_map`)

**Status:** 🟡 PARTIALLY FIXED. The DOMINANT bug (this doc's persistent-
`forwarding_ptr` root-remap flaw, deterministic ~12.5% on the repro) is **FIXED**
(see "Fix" below; `SteadyChurn @16m` ~12.5%→0 via the verifier's LOST check, no
regression on `binarytrees16`/`PromoteMixed` serial+parallel vs HotSpot). A
SEPARATE, rarer **concurrency race** remains 🔴 OPEN (~5%, timing-sensitive,
invisible to the post-collection verifier) — see "Residual". Still blocks
parallel-evac-default-on / the G1 default flip (Step 9/10 of
`docs/feature-designs/concurrent-gc-maturation.md`). The `task_58d60f7a` family
turned out to be TWO stacked bugs.

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

Parallel evac therefore stays **opt-in/experimental** and mixed stays serial until
this residual race is also fixed. Default G1 (serial) and Generational are
unaffected.
