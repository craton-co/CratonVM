# Fix: native-collections-streamsort

**ID:** native-collections-streamsort
**Owned file:** `native-collections/src/lib.rs`
**Finding:** P2 residual (Round 3 carry-over) — `native_stream_sorted_cmp`
(`Stream.sorted(Comparator)`, ~L8093) still used an O(n²) insertion sort
after the comparator-driven `Arrays.sort` / `Collections.sort` paths were
migrated to the stable fallible `merge_sort_fallible` helper in Round 3.

## What changed

Rewired `native_stream_sorted_cmp` to call `merge_sort_fallible`
(O(n log n), bottom-up, stable) with the comparator closure, **identically**
to how `sort_with_comparator` (`Collections.sort(List, cmp)`, L6557) already
invokes it. Only the sorting algorithm changed; the surrounding native
(arg parsing, `stream_elements`, `make_stream` output) is untouched.

Before (L8102–8122):

```rust
let mut elems = stream_elements(ctx, this);
let len = elems.len();
// Insertion sort — O(n²) but stable.
for i in 1..len {
    let key = elems[i];
    let mut j = i;
    while j > 0 {
        let cmp_result = comparator_compare(ctx, comparator, elems[j - 1], key)?;
        let cmp = match cmp_result { Some(Value::Int(v)) => v, _ => 0 };
        if cmp <= 0 { break; }
        elems[j] = elems[j - 1];
        j -= 1;
    }
    elems[j] = key;
}
make_stream(ctx, &elems)
```

After:

```rust
let mut elems = stream_elements(ctx, this);

merge_sort_fallible(ctx, &mut elems, |c, a, b| {
    match comparator_compare(c, comparator, *a, *b)? {
        Some(Value::Int(v)) => Ok(v),
        _ => Ok(0),
    }
})?;

make_stream(ctx, &elems)
```

## Why this is behavior-preserving

The closure passed to `merge_sort_fallible` is byte-for-byte the same one
`sort_with_comparator` uses (L6582–6587). Equivalence to the old insertion
sort on the three observable axes:

- **Stability.** `merge_runs_fallible` (L5923) takes the left run's element
  on `c <= 0` (ties keep the originally-earlier element first). The old loop
  broke out on `cmp <= 0`, also leaving equal elements in original order.
  Same stable contract `Stream.sorted` guarantees.
- **Comparator-exception propagation.** Both forms call `comparator_compare`
  with `?`, so a throwing comparator short-circuits the sort and propagates
  the `MethodCallFailed` out unchanged.
- **Non-Int comparator return.** Both treat any non-`Int` result as `0`
  ("equal"), so a malformed comparator behaves identically.

`comparator` is an `ObjectRef` (`Copy`), so capturing it by-value in the
closure is sound and matches the existing call site. `merge_sort_fallible` /
`merge_runs_fallible` are defined earlier in the same file (L5889 / L5923)
and were already linked into the build via the comparator sort paths, so no
new imports or symbols are introduced. The now-unused `len` binding was
removed with the loop.

## Complexity

O(n²) → O(n log n) comparator dispatches; one reused scratch buffer
(`Vec::with_capacity(n/2 + 1)`) instead of in-place shifting.

## Scope / policy

No synthetic-stub concern — this is a pure algorithm swap inside an existing
`Bridge`-category native. No registration, gating, or `NativeKind` change.

## Tests

Did not add a dedicated test: the `merge_sort_fallible` helper already has
direct coverage in the inline `#[cfg(test)]` module
(`merge_sort_fallible_sorts_and_is_stable`, L26998;
`merge_sort_fallible_propagates_comparator_error`, L27051), which now also
exercises the exact code path `native_stream_sorted_cmp` delegates to.

## Verification note

Did not run cargo/git (per instructions). Edit is surgical and mirrors an
existing compiling call site verbatim; compile confidence high.
