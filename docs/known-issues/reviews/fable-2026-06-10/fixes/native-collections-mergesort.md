# Fix: native-collections P2 — replace O(n²) insertion sort with stable fallible merge sort

ID: `native-collections-mergesort`
Report: `docs/reviews/fable-2026-06-10/native-collections.md` (finding **P2**, MEDIUM)

## Finding

Round 1 made `Collections.sort`/`Arrays.sort(Object[])`/`List.sort` *correct*
(natural order via `compare_via_compare_to`, comparator order via
`comparator_compare`), but implemented the sort core as an **O(n²) insertion
sort**. The comments justified this on the grounds that Rust's stable `sort_by`
takes an *infallible* comparator and a Java comparison can throw
(`ClassCastException`, or any exception from a user `Comparator.compare` /
`Comparable.compareTo`), which must short-circuit and propagate. For large lists
this is quadratic — the three sort entry points all shared the same O(n²) core.

## Root cause

Three call sites each contained their own hand-rolled insertion sort:

- `native_arrays_sort_objects` (`Arrays.sort(Object[])`) — natural order.
- `native_collections_sort` (`Collections.sort(List)` / `List.sort` natural) — natural order.
- `sort_with_comparator` (`Collections.sort(List, cmp)` / `List.sort(cmp)`) — comparator order.

All three needed a *fallible* comparator, which ruled out `sort_by`, so each used
insertion sort (O(n²)).

## Exact change

Added one shared helper pair and rewired all three call sites to it:

- **`merge_sort_fallible(ctx, items, cmp)`** — stable, iterative (bottom-up)
  merge sort, O(n log n). `cmp` is a closure
  `FnMut(&mut dyn NativeContext, &Value, &Value) -> Result<i32, MethodCallFailed>`;
  `ctx` is threaded in as a parameter on each call so the closure never has to
  borrow `ctx` itself. The first `Err` short-circuits the entire sort and is
  returned to the caller (same first-error semantics the old insertion sort had).
- **`merge_runs_fallible(...)`** — merges two adjacent sorted runs via a reused
  scratch buffer holding a copy of the **left** run; on a tie (`c <= 0`) it takes
  the left element first, preserving **stability**. The right run stays in place
  in `items`, and because the number of left elements written is exactly
  `mid - lo` (landing in `[lo, mid)`), the write cursor `out` never reaches an
  unconsumed right element — memory-safe and correct.

Call-site rewrites (behavior identical, only algorithm/complexity changed):

- `native_arrays_sort_objects`: insertion loop → `merge_sort_fallible(ctx, &mut items, |c,a,b| compare_via_compare_to(c,a,b))?`. Comparable-check + `ClassCastException` path unchanged (still runs before the sort).
- `native_collections_sort`: same substitution; Comparable pre-check unchanged.
- `sort_with_comparator`: insertion loop → `merge_sort_fallible` with a closure that calls `comparator_compare(c, comparator, *a, *b)?` and maps `Some(Value::Int(v)) => v` / `_ => 0` (preserving the old non-Int-return-treated-as-0 semantics).

Stability is preserved (`c <= 0` keeps the left/earlier element first, matching
the old `cmp <= 0 => break`), so output order is identical to Round 1 on every
input, including equal elements and the null-ordering corner cases handled inside
`compare_via_compare_to`. Updated the three explanatory comments from
"insertion sort O(n^2)" to "stable merge sort O(n log n)".

## Files touched

- `native-collections/src/lib.rs`
  - New: `merge_sort_fallible`, `merge_runs_fallible` (after `compare_via_compare_to`).
  - Rewired: `native_arrays_sort_objects`, `native_collections_sort`, `sort_with_comparator`.
  - Tests: added `merge_sort_fallible_sorts_and_is_stable` and
    `merge_sort_fallible_propagates_comparator_error` inside the existing
    `tests::lbq_blocking_tests` module (where `MockCtx` lives).

## Tests added

- `merge_sort_fallible_sorts_and_is_stable`: 15 elements with duplicate keys
  packed as `(key << 32) | original_index` into `Value::Long`, compared by key
  only. Asserts (a) keys non-decreasing across multiple merge passes
  (width 1→2→4→8…), (b) stability — equal-key elements keep ascending original
  index, (c) the output multiset equals the input (nothing lost/duplicated).
- `merge_sort_fallible_propagates_comparator_error`: a closure that always
  returns `Err(ClassCastException)` makes the sort return `Err` (short-circuit).

Could not run `cargo test` per task rules (separate build process). Tests use
only in-scope symbols (`merge_sort_fallible`, `Value::Long/Int`,
`MethodCallFailed::from(RuntimeError::ClassCastException{..})`, `MockCtx::new`),
all already used elsewhere in this module.

## Follow-up & risk

- **Risk: low.** Pure algorithm swap behind unchanged comparison/error plumbing;
  output order and `ClassCastException`-on-non-Comparable semantics are
  byte-identical to Round 1. Merge sort allocates one scratch `Vec<Value>`
  (≤ n/2) per sort vs. the in-place insertion sort, a minor, bounded allocation.
- The merge stops on the first comparator `Err`, leaving `items` partially
  merged (then discarded by the caller via early `?` return) — same observable
  behavior as before (the array/list is not written back on error).
- Out of scope (other findings in the same report): P1 LinkedBlockingQueue O(n²)
  poll; B1/V1 `tm_fast_table` GC gap; B2 `natural_compare` returning 0; B4/B5
  side-table growth/aliasing. Not touched.
