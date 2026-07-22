# Bug 14 — `@ParameterizedTest` empty arg stream → test under-run: `ArrayList.removeAll`/`retainAll` over-removal on a full backing array

## ROOT-CAUSED + FIXED (2026-06-12)

## Symptom
`FetchCollectorTest` ran **22 tests vs HotSpot's 129** (1 container failed with
`PreconditionViolationException: None of the supporting … ParameterizedTestExtension
provided a non-empty stream`). One `@MethodSource` (`testFetchWithOtherErrorsSource`)
returned an **empty** `Stream<Arguments>` on CratonVM (0 vs 107).

## Root cause (bisected)
The provider does:
```java
new ArrayList<>(Arrays.asList(Errors.values())).removeAll(<a few Errors>).stream()...
```
`ArrayList.removeAll(...)` (and `retainAll(...)`) **removed EVERY element** — not
just the listed ones — emptying the list, so the stream was empty.

Narrowed precisely (`apps/kafka/tests/repro/RA*.java`): the over-removal happens
**iff the backing array is exactly full (`capacity == size`)**, independent of
element content or how the list was built:
```java
List<E> l = new ArrayList<>(12); for (12) l.add(...);   // cap==size==12
l.removeAll(Arrays.asList(E.V0));   // CratonVM size=0 (expect 11); HotSpot 11
```
- `size <= 10` (so `cap > size`, with trailing nulls): correct.
- `size >= 11` (so `cap == size`): wipes the whole list.
- `removeAll(emptyCollection)`: correct (no writes).
- single `remove(x)`, `indexOf`, `contains`: correct on a full array.

The `removeAll`/`retainAll` natives (`native-collections/src/lib.rs`) did an
in-place single-pass compaction (read `buf[i]`, write `buf[w<i]` in the same loop).
On a full backing array that interleaving mis-evaluated the keep/remove predicate
for every element (`coll_elems.any(values_equal(...))` came out true for all),
collapsing the list to size 0. The exact VM-level interaction with a `capacity==size`
array was not fully isolated, but the trigger is precise and deterministic (not GC —
reproduces with `-Xmx8g`).

## Fix (`native-collections/src/lib.rs`)
Rewrote `native_al_remove_all` and `native_al_retain_all` as **two-pass**: read ALL
elements and decide keep/remove FIRST (read-only), then write the kept elements
back and set the size. This eliminates the read-during-write compaction that broke
on a full backing array.

## Status
- Committed `8bb77a77`. Repro: `RA*.java` (esp. `RAcap2.java`: cap12→11 after fix).
- Affected: `consumer.internals.FetchCollectorTest` (22→129) and any code doing
  `new ArrayList<>(coll).removeAll/retainAll(...)` on a >10-element collection —
  a very common pattern. Append more from the full run.
