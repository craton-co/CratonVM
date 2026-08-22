# `RTreeRangeGc` was THREE collection-native defects, and none of them was a root-collection gap

**Status: FIXED 2026-08-22.** All three land on `fix/gc-known-issues-20260822`.
`RTreeRangeGc` passes 25/25 on the default collector and 25/25 under
`--jdk-only` on one binary, and `SUITE=all` is **107 passed, 0 failed**.

This page opened as *"`RTreeRangeGc` reddens on the default collector — the gap
is old, the coverage is new"*. Its history is kept below because two of its
measurements were right and load-bearing, and its central inference was wrong in
a way worth recording.

---

## 1. What the original page concluded, and where that came from

It read one guard record:

```text
ERROR cratonvm::gc::guard: …and this is where that address stood in the OWNING
thread's own GC bookkeeping. obj="0x16e0a184d18" site="checkcast"
in_published_snapshot=true published_roots=893 last_publish_at_collection=1
collections_now=2 last_publish_pc=17 holder=<not found in frames>
in_blocked_region=false frames=2 top_frame=RTreeRangeGc.checkMap pc=44
```

and concluded a **root COLLECTION gap** — "the snapshot the collector marks this
thread from did not contain a slot the thread's frames hold".

**That record does not say that.** `op_checkcast`'s failure path calls
`report_reclaimed_receiver_forced` and `report_root_slice_provenance`
UNCONDITIONALLY, on every failing cast — `_forced` precisely so the re-served
face, which has a valid class id, is not suppressed. The record therefore
describes *where the address stood*, for any failed cast, and is silent on
whether reclamation had anything to do with it. Its own prose is written for the
`in_published_snapshot=false` case; the witness says **`true`**.

The exception one line further down in the same log is the whole diagnosis:

```text
ClassCastException: class RTreeRangeGc$V cannot be cast to class java.util.Map$Entry
        at RTreeRangeGc.checkMap(RTreeRangeGc.java:121)
```

A `V`. A live, correct, perfectly ordinary VALUE object. Nothing was stale.
`headMap(k, false).entrySet()` had handed back the view's 300 VALUES where its
contract is 300 ENTRIES, and `checkcast Map$Entry` was the first thing to
notice.

**The lesson, and it is the same one this repo has written down before:** a
diagnostic that fires on every instance of a failure class cannot, by itself,
classify one. Read the exception before the guard.

## 2. Defect 1 — a view carrier's KIND was inferred from data, and defaulted to "values"

`MAP_VIEW_CARRIERS` lists six carrier classes. Five hold values;
`java/util/TreeMap$EntrySet` holds entries. Which one a given carrier held was
decided by `al_view_holds_entries`, which read the CLASS OF ITS HEAD ELEMENT and
answered "values" for anything it could not classify. Two ordinary ways to reach
that verdict on a genuine `entrySet()`:

* **The view was EMPTY when the carrier was minted**, so there is no head
  element to read. No GC, no timing, no heap size — MEASURED against HotSpot
  25.0.3+9:

  ```text
  new TreeMap<>().entrySet()   held across two put()s, then iterated
  CratonVM   [v1, v2]          <- the values
  HotSpot    [k1=v1, k2=v2]
  ```

* **The head could not be classified during a moving collection**, which is the
  `RTreeRangeGc` face.

`vc_route` had the same mix-up one level up: it re-expresses a view-class
receiver over its source map, and its only rebuild was
`entries.map(|(_, v)| v)` — so an entry-set receiver its "did WE mint this"
marker test declined came back as values whatever the head element said.

Fixed by asking the CLASS. `view_carrier_holds_entries_by_class` answers exactly
and without reading the heap for all six carriers; the head-element reading
survives only as the fallback for the `java/util/ArrayList` carrier
`alloc_view_carrier` degrades to on a stripped image. `al_is_values_view` also
stopped calling an entry set a values view — `TreeMap$EntrySet`'s real
superclass is `AbstractSet`, which DOES override `equals`/`hashCode`, so equal
maps have equal entry sets and CratonVM was answering identity.

## 3. Defect 2 — the sync funnel relocates its own receiver

With the kind fixed, the vector still failed 12/12, now with

```text
AssertionError: RTreeRangeGc: headMap(k,false): 0 entries, expected 300
```

and passed **12/12 under `CRATONVM_ZGC_RELOCATE=0` on the same binary** — so
relocation-only, and a different defect.

`CRATONVM_DBG_VIEWRESYNC` (added for this) prints each range view's rebuild. The
failing run showed the fourth view rebuilt correctly at construction
(`pairs=400 kept=300`) at one address, and its `entrySet()` rebuild at a
DIFFERENT address. The view had relocated in between, which is fine. What is not
is what every content native did around the funnel:

```rust
let this = ...args[0];
tm_sync_native_state(ctx, this)?;          // dispatches compareTo over 400 keys
let pairs = tm_collect_pairs(ctx, this);   // <- the PRE-move `this`
```

`tm_sync_native_state` allocates — step 1 runs the key's real `compareTo` over
every entry of the backing map, step 2 allocates the deserialization mirror — so
it can move its own receiver. It returned `()`, so no caller could tell. The
following lookup then finds the map in the ADDRESS-KEYED side tables under its
from-space address, misses, and gets a fresh EMPTY state.

An empty view is the failure mode `W7-1-treemap-views-and-iterator-remove-contract`
exists to refuse, and the one a caller that only iterates cannot see.

Fixed by making the funnel return the refreshed receiver and marking it
`#[must_use]`, so each of the 34 call sites that ignored it became a compile
error rather than a rule to remember. Fifteen of those natives also read their
key out of the raw `args` slice on the line AFTER the sync — a plain Rust borrow
that nothing remaps — and now carry it across on a pin.

## 4. Defect 3 — six TreeSet range natives pinned addresses that were already stale

Under `--jdk-only` the vector still failed ~2 in 20:

```text
ClassCastException: class java.lang.Object cannot be cast to
                    class java.lang.Comparable    at RTreeRangeGc.java:210
```

`CRATONVM_DBG_CCE_BT` named it in one run:

```text
site=compare_via_compare_to  a=java/lang/Object(cid=0)  b=RTreeRangeGc$K(cid=434)
  compare_via_compare_to <- natural_compare <- tree_compare <- native_ts_sub_set
```

The BOUND is a good `K`. `a` is an ELEMENT, and `java.lang.Object` with
`class_id=0` is the free-list face of a block reclaimed while still referenced —
so the backing array it came out of had moved.

All six `TreeSet` range natives open with the same six lines: read `ts_state`,
then `try_alloc_declared_width` for the result set, then `alloc_ref_array` for
its elements, then publish. Everything read before those two allocations, and
both things allocated by them, were bare Rust locals across them. Each native
then opened its scan loop by pinning those locals — which pins NOTHING once a
relocation has already happened: a from-space address, pinned, stays a
from-space address. The elaborate per-iteration re-reads below were protecting a
pointer that was already wrong.

`ts_begin_range_result` does that prologue once, GC-safely, for all six.

## 5. What the original page got RIGHT, and it mattered

* **The `--Xmx 64m` arm is the one to judge on, three runs minimum.** Correct,
  and its own CORRECTION about the corpus arm was correct too.
* **`CRATONVM_ZGC_RELOCATE=0` and `-XX:+UseG1GC` as controls.** The
  `RELOCATE=0` control is what separated defect 2 from defect 1 in one command.
* **"The green was vacuous."** Also correct, and the strongest thing on the
  page: ZGC's refusal to compact meant this vector asserted 14,014 checks
  against a collector that never moved an object, for eleven days.

## 6. What it got wrong

* **The verdict**, for the reason in §1.
* **"Intermittent under the corpus."** The page's own re-correction, landed on
  `dev` hours before this fix and folded in here, already withdrew that: four
  corpus runs, two compatible arms each, gave `r13 FAIL/FAIL`, `r14 FAIL/PASS`,
  `r15 FAIL/FAIL`, `r16 FAIL/FAIL` -- seven of eight compatible observations
  FAIL and every strict one PASSED. It called that "deterministic and
  mode-dependent, with a rare flake", which matches the 12/12 compatible
  failure measured here exactly. `WORKER-5` reached the same reading from 12
  ABBA-interleaved runs, and their framing is the one worth keeping: **"flaky"
  is a property of a BINARY AND A HOST, not of a vector.** This lane's own
  strict arm bears that out from the other side -- 2 fails in 20 where theirs
  saw 0 in 6, same vector, different binary and load.
* **`--jdk-only` is CLEAN (2/2).** It is not; `WORKER-1-NOTE-1` measured 9
  pass / 3 fail over twelve runs the next day and was right. Two runs cannot
  see a 25% flake, and this page drew a mode conclusion from them.
* **"The affected path is the young/relocating one the other two collectors
  share and G1 does not take here."** G1 passes because G1 did not relocate
  these objects on this workload, not because it takes a different path.
* **"WORKER-4's `TreeMap.root` fix is DISPROVED, so it is the range-VIEW
  machinery."** The conclusion was right; the reasoning — a 3/3 vs 3/3 A/B over
  a defect that is at best 25% in one of its two modes — could not support it.

## 7. Measured, after

`cratonvm-tsrange-20260822`, one binary, `--Xmx 64m`, 25 runs per arm:

| arm | before | after |
| --- | --- | --- |
| default (ZGC) | 0 / 12 | **25 / 25** |
| `--jdk-only` | 2 fails in 20 | **25 / 25** |
| `-XX:+UseG1GC` | 12 / 12 | 12 / 12 |
| generational | 0 / N since 2026-08-20 | **12 / 12** |

`SUITE=all`: **107 passed, 0 failed**, where this page's own tables show 105/107
and 106/107.

## 8. Gate

`RJdkViews.entrySetStaysEntries` — the empty-then-populated entrySet, a range
view's entrySet asserted element by element, and the `equals`/`hashCode` half.
Asserted on CONTENT, never on `getClass()`: this VM's entries and HotSpot's are
different classes on purpose, and a class-name assertion would fail the cross-VM
diff for the wrong reason. It FAILS on the pre-fix binary and PASSES on HotSpot,
137 checks on both VMs.

The vector itself needed nothing: `RTreeRangeGc` was already registered, already
publishes its check count, and caught all three.

## 9. Left open

* **`regression-suite/known-flaky.txt` quarantines `RTreeRangeGc` at 3/12.**
  That row is now stale — 50 runs, two arms, zero failures. The row is removed
  here and `WORKER-1-NOTE-1` records why. Removing it cannot move the
  `25-linux` blast-radius baseline: that baseline is keyed on the SET of
  FAILING vectors per prefix, and a vector that passes contributes to no set
  either way.
* **The other `class_id`-based `java/lang/String` tests.** Not this page's
  defect, but the sibling page retired the same day
  (`corrupt-value-cell-producer-was-a-string-array`) found four doors of that
  shape and fixed them; the annotation-proxy renderers in `vm_exec` have the
  same shape and were not measured.

## Reproduce (the failing arm, on a pre-fix binary)

```bash
cratonvm --java-home "$JDK" --Xmx 64m -cp regression-suite/build RTreeRangeGc
CRATONVM_ZGC_RELOCATE=0 cratonvm --java-home "$JDK" --Xmx 64m -cp regression-suite/build RTreeRangeGc
cratonvm --java-home "$JDK" --Xmx 64m --jdk-only -cp regression-suite/build RTreeRangeGc
```

and, for the GC-free half of defect 1, `probes/TmViewEntryProbe` or four lines
of `new TreeMap<>().entrySet()` held across a `put`.
