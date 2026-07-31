# TreeMap/TreeSet snapshot walks dereferenced relocated `ObjectRef`s — **FIXED**

## Status
**FIXED** (2026-07-31, branch `fix/tm-range-pairs-staleref-20260731`). Reproduced
deterministically in isolation (3/3 before, 3/3 after), gated by a new
regression-suite class.

Found by auditing the Family-1 stale-`ObjectRef` defect class after the same bug
was fixed in `PriorityBlockingQueue` earlier the same day — see
`h2-suite-bugs/bug-h2-priorityblockingqueue-stale-objectref-classcastexception-FIXED.md`
for the family write-up and the reproduction technique.

## Symptom
```
Exception in thread "main" java/lang/ClassCastException:
  class java.lang.Object cannot be cast to class java.lang.Comparable
	at TmRangeStaleRefProbe.rangeViews(TmRangeStaleRefProbe.java:170)   // m.headMap(...)
```
The `java.lang.Object` is not a real object of the user's: it is whatever now
occupies the address a `TreeMap` key used to live at. `compare_via_compare_to`'s
`implements_comparable` guard is what turns the corrupted read into this
message rather than a silent wrong answer.

## Root cause

`tm_collect_pairs` snapshots a `TreeMap` into a bare Rust `Vec<(Value, Value)>`
of raw `ObjectRef`s. Every consumer then walks that `Vec` while running code
that allocates:

* `tree_compare` — for anything that is not a `String` or a homogeneous
  primitive wrapper this dispatches the key's **real, interpreted**
  `Comparable.compareTo` / `Comparator.compare`;
* `native_tm_put`, `alloc_ref_array`, `alloc_synthetic`, `alloc_live_entry`;
* `obj_to_display_string` (a real `toString()`), and a user lambda in `forEach`.

A young collection during any of those relocates every entry the walk has not
consumed yet. The collector remaps `native_pin_roots`; it does not and cannot
rewrite a Rust `Vec`. So from the first collection onward the rest of the walk
dereferences moved addresses.

`native_tm_for_each` had already been given the pin-and-refresh treatment
inline; nothing else had.

The `TreeSet` range views have the same defect in a smaller shape — they read
the element once,

```rust
let e = ctx.get_array_element(data, i);
let cmp = tree_compare(ctx, &comparator, e, to_elem)?;   // `e` can move HERE
data = ctx.read_native_pin(data_pin, data);              // data refreshed, `e` not
...
native_ts_add(ctx, &[Value::Object(Some(result)), e])?;  // stale `e`
```

— carefully refreshing `data`/`result`/`comparator`/the bound across the
comparison while leaving the one value actually being inserted stale.

## Sites fixed

Empirically reproduced (the gate covers these):

| site | hazard |
| --- | --- |
| `native_tm_head_map` / `tail_map` / `sub_map` | `pairs` entries stale across `tree_compare` + `native_tm_put` |
| `native_tm_head_map_inclusive` / `tail_map_inclusive` / `sub_map_inclusive` | same |
| `native_ts_head_set` / `tail_set` / `sub_set` (+ the three `_inclusive`) | `e` read before the comparison that moves it |
| `native_tm_key_set` | `alloc_synthetic` + `alloc_ref_array` between snapshot and use; `this` also stale |

Structurally identical, hardened by inspection in the same pass (an allocation
unconditionally sits between the snapshot and its use, so only timing decides
whether a collection lands there):

| site | hazard |
| --- | --- |
| `tm_collect_pairs` fast-mode branch | `tree_key_to_value` allocates per key (`create_string` / `Wrapper.valueOf`), moving both the not-yet-converted values and the already-boxed keys |
| `native_tm_entry_set` | `alloc_live_entry` per pair moves the remaining pairs *and* the entries already produced |
| `native_tm_to_string` | each element's real `toString()` |
| `native_tm_key_iterator` | `alloc_ref_array` + `alloc_synthetic` between snapshot and use |
| `native_ts_descending_set` | `native_ts_add` allocates; `data`/`result` never refreshed |
| `native_ts_write_object` | each `ObjectOutputStream.writeObject` runs interpreted serialization |

`make_view_list_of` and `alloc_live_entry` were already correct — both pin their
own inputs — which is why `native_tm_values` needed no change.

## The fix

A `PinnedPairs` helper next to `tm_collect_pairs`: pin the whole snapshot once
with `pin_value_slice`, then read each element back through its pin
(`read_pinned_elem`) immediately before every dereference. That is the only view
of the snapshot the collector keeps current. `refresh(&ctx, i, &mut k, &mut v)`
lets the range loops keep ordinary `Value` locals.

Cost is one pin per entry, on a walk that already performs one interpreted call
or one allocation per entry.

The `TreeSet` sites re-read the element from the (already pinned and refreshed)
`data` array immediately before `native_ts_add` instead of reusing the
pre-comparison copy.

## Reproduction / regression gate

`regression-suite/src/RTreeRangeGc.java`. A `TreeMap`/`TreeSet` of a two-long-field
`Comparable` whose `compareTo` allocates deliberately, so the collection lands
inside the native; 400 entries; `headMap`/`tailMap`/`subMap` in both the
`SortedMap` and `NavigableMap` shapes, plus `keySet` and `toString`, plus the
`TreeSet` twins. Every view is checked for intact keys *and* values, ascending
order, and exact membership.

`run.sh` runs it with **`--Xmx 64m`**. On the default heap no collection happens
during the walk at all and the class passes on a broken VM. Note the difference
from `RPriorityQueueGc`: this one does **not** need `--nojit` — it reproduces
with the JIT on, so the gate leaves the default compiling configuration under
test.

| binary | `ONLY=RTreeRangeGc bash regression-suite/run.sh` |
| --- | --- |
| `origin/dev` @ `9fcd1b63f2` | **FAIL** `rc=1` (ClassCastException out of `headMap`), 3/3 |
| with this fix | **PASS**, 3/3, output matches HotSpot |

## Verification

* `cargo test -p cratonvm-native-collections` — 156 tests, 0 failures.
* Full `regression-suite/run.sh` on the fixed binary: **20 passed, 0 failed**.
* `cargo clippy -p cratonvm-native-collections` clean. `cargo fmt --check` output
  is byte-identical to the pre-change tree apart from one line-number shift —
  the remaining diffs are pre-existing on `dev`, none introduced here.

## Not done
`collect_entries_any` / `collect_keys_any` (lines ~8710 / ~9120) also funnel
`tm_collect_pairs` results into caller-owned walks. Their callers were not
audited here.
