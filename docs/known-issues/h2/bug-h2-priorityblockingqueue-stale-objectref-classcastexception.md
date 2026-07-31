# `PriorityBlockingQueue`'s native `offer()` doesn't refresh its backing-array/element `ObjectRef`s across a re-entrant `compareTo()` call — corrupts concurrent H2 MVStore compaction

## Status
**OPEN** — root-caused via direct code inspection, matching an
already-established, previously-fixed bug family in a sibling collection
(`TreeMap`'s `tm_binary_search`). Not yet fixed or empirically reproduced
in isolation. Found while investigating H2 suite regressions in the
twelfth-pass `TestUpgrade` session's follow-up full-suite run (2026-07-30/31).

## Severity
**MEDIUM** — requires concurrent access to a `PriorityBlockingQueue` of a
custom `Comparable` type under active GC pressure to manifest; H2's own
MVStore `FileStore` uses exactly this shape internally for tracking
removed pages during compaction (`FileStore$RemovedPageInfo`), under
concurrent multi-threaded write load.

## Affected test classes (H2 suite, this run)
- `org.h2.test.db.TestCompatibility` (`testConcurrentAutoIncrement`) — direct
  hit, `ClassCastException` inside `RemovedPageInfo.compareTo`.
- `org.h2.test.db.TestMultiThread` (`testConcurrentInsert`) — very likely a
  downstream symptom of the same mechanism (an `ExecutionException`
  wrapping "The database has been closed", consistent with a background
  MVStore panic closing the store out from under a concurrent insert — not
  independently confirmed this session, see "Related" below).

## Symptom
```
org.h2.jdbc.JdbcSQLNonTransientException: General error: "General error:
  ""org.h2.mvstore.MVStoreException: java.lang.ClassCastException: java.lang.Object
   cannot be cast to org.h2.mvstore.FileStore$RemovedPageInfo [2.4.249/3]"" [50000-249]"
	at org/h2/test/db/TestCompatibility.lambda$testConcurrentAutoIncrement$0(TestCompatibility.java:498)
	...
Caused by: java/lang/ClassCastException: java.lang.Object cannot be cast to
  org.h2.mvstore.FileStore$RemovedPageInfo
	at org/h2/mvstore/FileStore$RemovedPageInfo.compareTo(FileStore.java:2172)
	at java/util/concurrent/PriorityBlockingQueue.add(PriorityBlockingQueue.java:450)
	at org/h2/mvstore/FileStore.accountForRemovedPage(FileStore.java:2105)
```
`RemovedPageInfo.compareTo(Object o)` casts its argument `o` to
`RemovedPageInfo` — real HotSpot JDK25 passes this test, so this cast
should never fail; something is handing the real method a value that isn't
actually a `RemovedPageInfo` instance.

## Root cause (via code inspection)
`native-collections/src/lib.rs`'s `native_pbq_offer` (backing
`PriorityBlockingQueue.offer(Object)`, which real bytecode `add(E)` calls
through) delegates to `pbq_offer_locked`, which does a sorted-array binary
search to find the insertion point:

```rust
fn pbq_offer_locked(ctx: &mut dyn NativeContext, this: ObjectRef, elem: Value) -> MethodCallResult {
    ...
    let arr = match ctx.get_field(this, PBQ_FIELD_DATA) { ... };   // read ONCE, before the loop
    let comparator = Value::Object(None);
    let mut low = 0; let mut high = size as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_elem = ctx.get_array_element(arr, mid);
        let cmp = tree_compare(ctx, &comparator, mid_elem, elem)?;   // <-- can run REAL, interpreted Comparable.compareTo
        ...
    }
    // shift + insert, STILL using the same `arr` captured before the loop:
    for i in (low..size as usize).rev() {
        let v = ctx.get_array_element(arr, i);
        ctx.set_array_element(arr, i + 1, v);
    }
    ctx.set_array_element(arr, low, elem);   // `elem` is the ORIGINAL argument, never refreshed either
    ...
}
```

`tree_compare` → `natural_compare` → `compare_via_compare_to` dispatches
the element's **real, interpreted** `compareTo()` method whenever the
element isn't a `String` or a homogeneous primitive wrapper — exactly
`FileStore$RemovedPageInfo`'s case. A real interpreted call can allocate
(directly, or via a concurrently-running GC on another thread pausing/
moving objects while this thread is inside the call), which under a moving
collector can invalidate any `ObjectRef` captured *before* the call and
never re-read afterward.

`arr` (the backing array reference) and `elem` (the value being inserted)
are both captured once, before the loop starts, and are used again *after*
every `tree_compare` call — including the final `set_array_element(arr,
low, elem)` that actually stores the new entry — without ever being
re-read through `ctx` again. If a moving GC relocates the backing array or
the element between one of the binary-search `tree_compare` calls and this
final write, the write either lands on a stale/reclaimed address, or
(more consistent with the observed symptom) inserts a stale `elem`
`ObjectRef` into the array — a slot that a **later** `offer()`/`poll()`
call reads back as whatever object now occupies that address after GC
reclaims/reuses it, which can be an unrelated `Object` — exactly matching
"java.lang.Object cannot be cast to RemovedPageInfo".

This is the **same bug family already found and fixed once in this
codebase**, in the sibling `TreeMap`/`TreeSet` array-mode binary search
(`tm_binary_search`, same file) — that function's own doc comment
(directly above it) explicitly describes this exact hazard ("Family-1
stale-ObjectRef fix, 2026-07-13: `tree_compare` below can dispatch to an
arbitrary user-supplied `Comparator`... a full interpreted call that can
allocate and trigger a moving GC. Both `owner`... and `data`... must be
re-read after every comparator invocation, not just once at entry") and
was fixed to re-read and re-pin both across every comparator call.
`pbq_offer_locked` was apparently never given the same treatment.

`native_pbq_poll`/`pbq_poll_locked` do not call `tree_compare` at all (pure
positional shift, no comparisons), so they are not subject to this same
hazard — this looks scoped to the `offer`/`add` insertion path specifically.

## Suggested fix
Mirror `tm_binary_search`'s existing pattern in `pbq_offer_locked`: pin
`this`/`arr`/`elem` before the binary-search loop, re-read all three
(and `arr` specifically, since `pbq_ensure_capacity` can also itself
replace the array) through their pins after every `tree_compare` call,
and use the freshly-re-read `arr`/`elem` for the post-loop shift-and-insert
instead of the pre-loop captures.

## Not yet done
- No isolated, H2-independent repro was written this session (unlike the
  already-fixed `tm_binary_search` sibling bug, which has one) — the
  evidence here is code inspection plus a matching real-world symptom, not
  a confirmed minimal reproduction. A repro would look like: many threads
  concurrently `offer()`-ing a custom `Comparable` into one
  `PriorityBlockingQueue` while forcing GC pressure (e.g. `System.gc()` in
  a loop on another thread, or simply enough concurrent allocation), then
  watching for a `ClassCastException`/corrupted read on `poll()`.
- The fix itself was not attempted/verified this session.

## Related
`org.h2.test.db.TestMultiThread`'s failure in the same run
(`testConcurrentInsert`, `ExecutionException` wrapping "the database has
been closed") was **not** independently traced this session, but its shape
(a background MVStore panic mid-concurrent-write) is consistent with
either this bug or the already-closed, unrelated
`bug-h2-mvstore-insert-loop-perf-hang.md`/chunk-reclaim-race family
documented in `bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md` —
worth checking against this bug specifically before assuming it's the same
cause.
