# Lazy/live map views — FIXED 2026-08-23

**Status: FIXED 2026-08-23.** Design A of the plan page retired with this one
(`known-issues/perf/lazy-map-views-plan-and-blockers-20260822.md`, 2026-08-22)
is implemented and measured. The three blockers that page recorded are answered
below — one was already closed
by the `modCount` work that landed between the two dates, one turned out not to
apply to design A at all, and one is a real constraint that design A routes
around rather than solving.

`keySet()` on a 1000-entry `LinkedHashMap` went from **1674.5 µs/call to
2.5 µs/call**, and `keySet().size()` — the shape Spring's `getPropertyNames()`
uses — from **3125 µs/call to 4.5 µs/call**, on one binary with
`CRATONVM_MAP_VIEW_CACHE` as the A/B.

## What was wrong

`map.keySet()` → `native_map_key_set` / `native_lhm_key_set` →
`make_view_set_of`, which allocates a carrier, allocates a backing map and
inserts every key through `native_map_put` — hashing each key through the full
`map_hash_key` triage and probing the bucket chain for duplicates.

That was the known half. The other half was worse: for a non-STATIC view kind,
`resync_view_set` **rebuilt the whole backing again on every read**. It
allocated a fresh bucket array and re-inserted every key, from `size()`,
`iterator()` and nine other call sites. So Spring's

```java
StringUtils.toStringArray(map.keySet())   // getPropertyNames()
```

rebuilt a 1000-entry hash map twice per call, 101 910 times per run of
`ConfigurationPropertySourcesTests`.

The design intent was right — a non-STATIC view is meant to be *live*, and
resyncing is how it stays live. It was the implementation of "live" that was
O(n)-per-read instead of O(1).

## The fix, in two halves

### Half 1 — the read no longer rebuilds

"Live" only requires the contents to agree with the source **at the moment of
the read**, and a monotonic generation on the source answers that in one field
load. The view backing carries the source generation it was last built from
(`VIEW_BACKING_SRC_GEN_SLOT`, slot 16, placed ABOVE both existing marker slots
rather than below them — one extra slot per backing, in exchange for not
assuming that no future `java/util/HashMap` will declare a field at slot 13).
A resync whose source has not moved since returns immediately.

The guard may **over**-invalidate freely; it must never **under**-invalidate.
`remove(k1); put(k2)` leaves the size unchanged, which is why the signal is a
counter and not the size.

Which `(kind, source)` pairs have a usable generation at all is the whole
safety argument, and it lives on `view_source_generation`:

| refused | why |
|---|---|
| entrySet views (both kinds) | contents include VALUES, and a value-replacing `put` deliberately does not bump `modCount` — HotSpot does not either (`put=5 remove=6 replace=6` in `probes/MapModCountProbe2`) — so a generation guard would UNDER-invalidate |
| both STATIC kinds | the source is `java/util/Properties`, half of whose keys live in a Rust side-table that no `modCount` tracks |
| `ConcurrentHashMap` sources | CHM mutations bump the SEGMENT's `modCount` (`native_chm_put` delegates to `native_map_put` on the segment), never the CHM's own, so its generation is frozen for the life of the map |
| a source with no readable `modCount` | there is no signal at all |

What that leaves — `HashMap`, `LinkedHashMap`, `Hashtable` and `TreeMap` — is
the set whose every structural mutator was audited to reach
`bump_map_mod_count`.

### Half 2 — `keySet()` / `entrySet()` return the instance the map already has

The elision killed the per-read rebuild; the per-call **construction** was
still 1674 µs. A live view can simply be handed out again, because every read
through it resyncs. That is also what HotSpot does — `AbstractMap` declares
`keySet` and `values`, `HashMap` declares `entrySet`, and each is the "return
the one instance" slot — so this restores the identity guarantee
`map.keySet() == map.keySet()`, which this VM did not have.

The read side **validates rather than trusts**: the field is a real JDK slot
other code may write, so the candidate must carry a view backing whose source
is this map by identity and whose kind matches. Anything else is ignored and a
fresh view is built, which is what happened before this cache existed.

Refused here, each for its own reason:

* **the STATIC kinds.** Not for the elision's reason. A STATIC view resyncs by
  [`adopt_fresh_view_backing`], which asks the SOURCE for a fresh view and
  adopts its backing — if the accessor returned the cached instance, the view
  would adopt its own backing and never refresh again. That is a liveness
  break, not a slow path.
* **the `Hashtable`/`Properties` family**, whose accessors hand back a
  `Collections$Synchronized*` wrapper and half of whose keys live in a side
  table.
* **`values()`**, whose carrier is a list with a different backing scheme that
  the validation above does not cover.

## Measured

One binary, `CRATONVM_MAP_VIEW_CACHE=0` as the OFF arm, `probes/KeySetBench`,
`LinkedHashMap` of 1000 entries, 2000 outer iterations. The engagement counters
(`CRATONVM_DBG=map-view-cache`) are printed beside each number, because a
wall-clock figure quoted without them cannot say whether the fast path ran:

| rung | OFF µs/call | ON µs/call | ratio | engagement (ON) |
|---|---:|---:|---:|---|
| `viewOnly` — `keySet()` and nothing else | 1674.5 | **2.5** | 670x | `view_reused=2000 view_built=1` |
| `sizeOnly` — `keySet().size()` | 3125.0 | **4.5** | 694x | `view_reused=2000 resync_skipped=2000` |
| `perCall` — `keySet()` then iterate (what Spring does) | 5949.5 | **1489.5** | 4.0x | `view_reused=2000 resync_skipped=2000` |
| `hoisted` — `keySet()` once, iterate inside the loop | 2939.5 | **1152.0** | 2.6x | `resync_skipped=2000` |
| `mapSize` — `map.size()`, the control | 2.5 | 2.5 | — | — |

`sizeOnly` is the Spring shape and it is the one that collapses.
`hoisted`'s residual 1152 µs is the ITERATION cost of 1000 elements
(~1.15 µs/element) and is not this fix's to remove — see "What is left" below.

## How it is known to be right

* **`CRATONVM_VERIFY_MAP_VIEW_CACHE=1`** takes the elision decision, then
  rebuilds anyway and compares, panicking on divergence. The comparison is over
  sorted IDENTITY HASHES, not addresses: the rebuild allocates, so under a
  moving collector every element relocates between the two snapshots and a
  pointer comparison would report a divergence on every single call. This turns
  the soundness claim into something the tree checks on every read rather than
  an argument about which mutators move a counter.
* **`probes/MapViewBehaviourProbe`** — the behavioural gate for view liveness —
  is byte-identical to HotSpot with the switch ON and OFF. It earned its place
  during this change; see the near-miss below.
* `probes/MapModCountProbe`, `probes/MapModCountProbe2`,
  `probes/ViewShapeProbe`, `probes/ItrFieldProbe`.
* The 136 `native-collections` unit tests, `cratonvm-vm`'s 2606, and the fast
  regression suite.

### The near-miss, recorded because the next one will have the same shape

The first cut of half 2 put the `entrySet` cache check into
`native_map_values` — the patch anchor matched an identical two-line preamble
one function earlier. `m.values()` therefore returned the ENTRY SET after any
`entrySet()` call.

It compiled. All 136 `native-collections` unit tests passed. The regression
suite passed. `MapViewBehaviourProbe` caught it on ONE row —
`hm.values.toString.len` 3 → 5, i.e. `[3]` where HotSpot says `[3]` and we said
`[a=1, b=2]` — and bisecting that row against the elision-only binary put the
divergence on the instance cache in a single run.

A unit test suite that covers the functions cannot catch an edit that lands in
the wrong function, because the wrong function is also covered and still
passes. Only a probe that asks "does a values view still contain values"
could.

## The three blockers, answered

**Blocker 1 — the invalidation generation was not maintained for
LinkedHashMap.** Closed before this work started, by `539d962f1` (LHM),
`cf714cf12` / `420d5117a` (TreeMap) and the fail-fast door map
(`map-view-iteration-failfast-doors-20260823.md`). The audit this page asked
for was re-run against the current tree and found **one remaining gap**, fixed
here: an access-ordered `LinkedHashMap` reorders on `get()` through
`lhm_move_to_tail` WITHOUT bumping `modCount`, where HotSpot's
`afterNodeAccess` opens with `++modCount`. That is load-bearing twice over —
it is a fail-fast divergence in its own right, and without it a keySet view
would keep serving the pre-access order for as long as no key was added or
removed.

The same page's "worth checking while in there" is also closed:
`bump_map_mod_count`'s doc claimed to be the invalidation generation for "the
bounded String-node lookup cache below", and no such cache exists. Its doc now
names its two real readers (`map_itr_check_comod` and
`view_source_generation`) and the audited mutator list.

**Blocker 2 — the backing escapes to JDK bytecode.** Real, and it is why
design A was chosen. Design A never hands out an unpopulated backing: the
backing is always fully materialised, and what changes is only whether it is
REBUILT. `keySet().spliterator()` reading `getfield map.table` / `map.modCount`
/ `map.size` therefore sees exactly what it saw before.

**Blocker 3 — the accessor is nearly, but not quite, a chokepoint.** Does not
apply to design A, which needs no lazy materialisation and so needs no
`hs_backing_map` mutable twin. It stays open as a precondition for design B.

## What is left

* **Design B** (lazy backing + source-delegating reads) is still the better end
  state and is unblocked by nothing here. Its precondition remains blocker 3.
* **`values()` views** get neither half: their carrier is a list with a
  different backing scheme. `probes/KeySetBench` has no `values` rung, so there
  is no number for what that costs.
* **entrySet READS still rebuild.** Only the construction is elided for them.
  Closing that needs a second generation that moves on a value-replacing `put`
  as well as on a structural change — which is another mutator audit, of the
  kind whose failure mode is a silently stale collection, and is not worth
  doing until something measures the entrySet read path as a wall.
* **Map-view ITERATION is ~1.15 µs/element** (`hoisted`, above). That is a
  separate wall from this one and this fix does not touch it.

## Reproducing

```bash
CV=<bin>
for r in viewOnly sizeOnly perCall hoisted mapSize; do
  CRATONVM_DBG=map-view-cache $CV --cp <probes> KeySetBench $r 2000 1000
  CRATONVM_MAP_VIEW_CACHE=0 CRATONVM_DBG=map-view-cache \
    $CV --cp <probes> KeySetBench $r 2000 1000
done
CRATONVM_VERIFY_MAP_VIEW_CACHE=1 $CV --cp <probes> MapViewBehaviourProbe
```
