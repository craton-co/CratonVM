# Fix note — `native-collections`

Owned file: `native-collections/src/lib.rs` (only file edited).

Findings addressed: **B1/V1** (verified already fixed — no change needed),
**B2**, **B3**, **P1**.

---

## B1 / V1 — fast-mode TreeMap values not GC roots / not remapped

### Finding
`tm_fast_table` (`Mutex<HashMap<usize, BTreeMap<TreeKey, Value>>>`) holds the
authoritative `Value` for fast-mode TreeMaps; the report says it is omitted from
`gc_scan_collection_overlay_roots` / `gc_update_collection_overlay_refs` → the
stored object values can be collected or left dangling after a moving GC.

### Root cause / status
**Already fixed in the current tree — no edit required.** Both GC hooks are now
defined in terms of a single shared funnel `for_each_overlay_ref`
(`lib.rs` ~L16257), and that funnel explicitly iterates `tm_fast_table` values
(`lib.rs` ~L16292–16300, with a doc comment naming the "B1/V1 use-after-free").
`gc_scan_collection_overlay_roots` (~L16316) and
`gc_update_collection_overlay_refs` (~L16323) both call `for_each_overlay_ref`,
so fast-mode values are reported as roots and repointed.

The companion test harness `native-collections/tests/gc_relocation_harness.rs`
(owned by another agent; shows as `M` in git status) already asserts this:
`fast-mode TreeMap value must be reported as a GC root` and
`...must be repointed to its relocated address ... (B1/V1)`, plus a cross-table
`for_each_overlay_ref` enumeration test. I did not duplicate or re-implement this
to avoid a conflicting parallel solution.

### Exact change
None. Verified-only.

---

## B2 — `natural_compare` returns 0 for arbitrary `Comparable` objects

### Finding
For an `(Object, Object)` pair that is neither a String nor a homogeneous
primitive-wrapper, `natural_compare` fell through to `_ => Ok(Some(Value::Int(0)))`
("equal") instead of dispatching `compareTo`. Reached by `tree_compare` for
comparator-less `TreeMap`/`TreeSet` of a custom `Comparable` key → ordering
collapses, entries lost, dedup broken.

### Root cause
The inner `match (fa, fb)` (field-0 wrapper values) had no Comparable fallback;
the existing correct helper `compare_via_compare_to` (used by
`Arrays.sort(Object[])`) was never wired into this path.

### Exact change (`native-collections/src/lib.rs`)
In `natural_compare` (~L11062), the inner-match catch-all changed from
`_ => Ok(Some(Value::Int(0)))` to
`_ => Ok(Some(Value::Int(compare_via_compare_to(ctx, a, b)?)))`.
`a`/`b` are the original `&Value` params (objects), so
`compare_via_compare_to` hits its `(Object(Some), Object(Some))` arm and invokes
`compareTo`. A thrown `compareTo` propagates via `?` (through `tree_compare`,
which already returns `Result`). The final bare-primitive-mismatch catch-all of
`natural_compare` is intentionally left returning 0 (no `compareTo` dispatch is
possible on bare unboxed primitives; those don't occur for TreeMap/TreeSet keys).

---

## B3 — `Collections.sort(List)` only sorts by string key

### Finding
`native_collections_sort` built `Vec<(String, Value)>` with key
`read_string(obj).unwrap_or_default()` and sorted by that string → `List<Integer>`
/ `List<Date>` / any non-String Comparable got an all-empty key and was returned
**unsorted**.

### Root cause
The 1-arg path was never updated when `Arrays.sort(Object[])` was fixed to use
natural ordering.

### Exact change (`native-collections/src/lib.rs`)
Rewrote `native_collections_sort` (~L6005) to mirror `native_arrays_sort_objects`:
snapshot the backing array into `Vec<Value>`, verify every non-null element is
`Comparable` (else throw `ClassCastException`, matching the JDK and
`Arrays.sort`), then insertion-sort dispatching through `compare_via_compare_to`
(fallible-comparator-safe; O(n^2) like the sibling sorts — see perf note). This
also fixes the null-comparator path of `native_collections_sort_comparator`,
which delegates here.

---

## P1 — `lbq_poll_locked` is O(n) per poll → O(n^2) FIFO drain

### Finding
`lbq_poll_locked` shifted every remaining element left by one on each dequeue,
making an N-element drain O(n^2).

### Root cause
The backing `Object[]` (in `LBQ_FIELD_HEAD`) kept the live elements packed at
`[0, size)` with the head always at index 0, so every poll had to compact.
`LBQ_FIELD_TAIL` was an unused, always-0 slot.

### Exact change (`native-collections/src/lib.rs`)
Repurposed `LBQ_FIELD_TAIL` as an integer **head index**: live elements now
occupy `[head, head + size)`; logical element `i` lives at array index
`head + i`. Poll reads `arr[head]`, clears it, and advances `head` — **O(1)**.
Added helpers `lbq_head` / `lbq_set_head` and a layout doc comment near the
`LBQ_FIELD_*` constants (~L21878). The window is a *linear* (non-wrapping)
buffer that drifts rightward; `lbq_ensure_capacity` (~L22253) now compacts the
live window back to index 0 in place before any append (and grows only when a
compacted window still won't fit), so the tail never runs off the end and the
array does not grow unbounded.

Every LBQ array-touching site was offset by `head` (and `head` reset to 0 when
the queue empties / is cleared / compacts):
- `lbq_poll_locked` (O(1) now), `native_lbq_offer` (append at `head+size`),
  `native_cld_offer_first` (O(1) prepend when `head>0`; else compact + shift),
  `native_lbq_poll_last`, `native_lbq_peek`, `native_lbq_peek_last`,
  `native_lbq_contains`, `native_lbq_remove` (shift over removed slot),
  `native_lbq_to_array`, `native_lbq_iterator`, `native_lbq_clear` (resets head).

This native backs `LinkedBlockingQueue`, `ArrayBlockingQueue`,
`ConcurrentLinkedQueue`, and `ConcurrentLinkedDeque`; all share the same layout
so all benefit. `PriorityBlockingQueue` (separate `PBQ_*` fields) is untouched
(out of scope; heap removal is inherently a shift/sift).

---

## Files touched
- `native-collections/src/lib.rs` — B2, B3, P1 (B1/V1 verified already fixed).
- `docs/reviews/fable-2026-06-10/fixes/native-collections.md` — this note.

## Tests added
- `head_index_fifo_across_buffer_wrap` (inline `#[cfg(test)]`, in the existing
  LBQ test sub-module): deterministically forces head drift + an in-place
  compaction on a bounded buffer and asserts strict FIFO drain order and that
  `head` resets to 0 when empty — directly validating the P1 head-index path.
  Reuses the existing `make_lbq` / `offer_must_succeed` / `lbq_size` helpers and
  the heap-backed `MockCtx`. The pre-existing concurrent FIFO test
  (`concurrent_put_poll_no_lost_values`) and the bounded `put_blocks_at_capacity`
  test also exercise the new compaction path.

## Follow-up & risk
- **P1 risk**: the largest change. It rewrites the entire LBQ family's index
  arithmetic. I traced the existing LBQ tests by hand (poll-empty, blocked-put,
  concurrent FIFO drain, clear-unblocks-put) against the new layout; all hold.
  The `ABQ`/bounded path relies on `offer_bool` rejecting at capacity, so the
  offer path's `needed = size+1 <= capacity = old_len` always lands in the
  no-alloc compaction branch. Could not `cargo build` per instructions —
  conservative, mirrors existing types/patterns.
- **B2 residual**: `Comparator.naturalOrder()` / `comparing(keyExtractor)` now
  also order custom Comparables correctly (they route through `natural_compare`).
  The final bare-primitive catch-all still returns 0 by design.
- **Perf (out of scope here)**: `Collections.sort` / `Arrays.sort(Object[])` /
  `sort_with_comparator` remain insertion sort (O(n^2)) — P2 in the report; a
  fallible-comparator merge sort would restore O(n log n) but is a larger change
  and not part of this finding set.
- `LBQ_FIELD_TAIL` is now load-bearing (head index). The only other writers were
  the two init paths (both set it to 0 = head 0); comment updated accordingly.
