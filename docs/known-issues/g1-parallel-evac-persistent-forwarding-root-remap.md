# G1 parallel evacuator drops a root-referenced object (persistent `forwarding_ptr` vs per-cycle `pointer_map`)

**Status:** 🔴 OPEN — fully root-caused, no working fix yet. Blocks
parallel-evac-default-on and the G1 default flip (Step 9/10 of
`docs/feature-designs/concurrent-gc-maturation.md`). The `task_58d60f7a` family.

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

| Attempt | Result | Why it fails |
|---|---|---|
| Clear `forwarding_ptr` for all `pointer_map` keys at cycle END | 28/30 bad | removes the redirect that masks the stuck roots; the LOST object is not even a key |
| Clear every CSet object's `forwarding_ptr` at cycle START | 28/30 bad | same, + re-copies the stale from-space object the root is stuck on |
| Record fast-path hits `(old, existing)` into `pointer_map` | 8/30 (~baseline) | `existing` can be a STALE cross-cycle redirect (collected since) → roots remapped to freed memory |

## The proper fix (sketch — a focused redesign)

The parallel evacuator needs a **per-cycle** forwarding notion so a fast-path hit
is unambiguously a this-cycle forward AND every forward (including fast-path
hits) is recorded so roots are remappable. Options:

- generation-tag the `forwarding_ptr` (store `(cycle_id, new)`; treat a
  stale-cycle tag as not-forwarded), then record every this-cycle hit; or
- never persist `forwarding_ptr` across cycles — clear it for *every* evacuated
  object at cycle end (not just self-forwards) AND record fast-path hits, so the
  roots are unstuck the same cycle the redirect is removed (the two failed
  clear-only / record-only attempts must be combined, and validated that they
  keep roots on the FINAL live copy, not an intermediate); or
- drop the header-field dedup entirely and use a concurrent `pointer_map`
  (`DashMap` / sharded) like the serial path, accepting the contention.

Any candidate must be validated with the repro above (0 corruption over ≥30 runs)
**and** `PromoteMixed` serial≡parallel≡HotSpot + the diverse soak, since the
forwarding scheme is shared by young and mixed.
