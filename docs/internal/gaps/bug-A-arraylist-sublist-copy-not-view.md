# Bug A — `ArrayList.subList()` returns a copy, not a live view  ✅ FIXED

| | |
|---|---|
| **Severity** | High (silent data corruption; broad blast radius) |
| **Kind** | Wrong result (no exception) |
| **Surfaced by** | `org.apache.kafka.clients.consumer.internals.AcknowledgementsTest` (3 failing tests) |
| **CratonVM** | FAIL · **HotSpot** OK |
| **Status** | **FIXED** on dev — synthetic `ArrayListSubList` backed view + structural-mutation delegation (`fix/asl-structural-delegation`) |
| **Recommendation** | Fix (done) |

## Symptom

`AcknowledgementsTest` failed 3 assertions under CratonVM, all of the form
`expected: <1> but was: <3>` / `<2>`:

- `testSingleAcknowledgeTypeWithinLimit` — 3 contiguous `ACCEPT`s should collapse to one
  acknowledge-type entry; CratonVM kept all three.
- `testMultiGap`, `testNoncontiguousBatches` — same pattern.

## Root cause

`Acknowledgements.getAcknowledgementBatches()` collapses a run of equal acknowledge
types with the idiomatic range-delete:

```java
batch.acknowledgeTypes().subList(1, batch.acknowledgeTypes().size()).clear();
```

Under CratonVM this is a **no-op**. `java.util.ArrayList.subList(from,to)` was natively
overridden (`native_al_sub_list` in `native-collections/src/lib.rs`) to allocate a **new
`ArrayList` and copy the range into it**. The returned list is therefore a detached
snapshot: every mutation through the sublist (`set` / `add` / `remove`, and
`subList().clear()`) writes to the copy and is silently dropped instead of writing
through to the parent — violating the `List.subList` live-view contract.

So `[1,1,1].subList(1,3).clear()` left the list as `[1,1,1]` (HotSpot: `[1]`), and the
batch kept 3 acknowledge types → `assertEquals(1, …acknowledgeTypes().size())` saw 3.

## Minimal repro (no Kafka, no Mockito)

```java
List<Integer> l = new ArrayList<>(); l.add(1); l.add(2); l.add(3);
l.subList(1, 3).clear();
// HotSpot: [1]      CratonVM (pre-fix): [1, 2, 3]
```

Probes (`ksuite/repro/SubProbe*.java`) confirmed the returned object's class was
`java.util.ArrayList` (a copy), not a view, and that **all** sublist mutations (`clear`,
iterator `remove`, `removeAll`, `removeIf`, `remove(int)`, `set`, `add`) failed to reach
the parent, while parent-level ops worked. It is **not** JIT-specific (fails under
`--nojit` too).

## Fix

`subList(from,to)` returns a synthetic backed **view** — `cratonvm/internal/ArrayListSubList`
(`ASL`) — that holds `(parent, offset, size, expectedParentSize)` and shares the parent's
`elementData`, rather than a detached copy:

- **Reads** (`get`/`size`/`iterator`/`toArray`/`contains`/`stream`/…) index straight into
  the parent's backing array at `offset+i`, or delegate to a fresh snapshot `ArrayList`
  (`asl_delegate_snapshot`); each first does an `asl_check_comod` (parent-size-vs-expected,
  mirroring the JDK's `checkForComodification`).
- **Positional write** (`set(i,v)`) writes through to the parent slot `offset+i`.
- **Structural mutation** (`add`/`remove`/`clear`/`addAll`/`removeIf`) now delegates through
  `asl_delegate_mutating`: snapshot the slice into a real `ArrayList`, run the real-JDK
  mutator on it (reusing its exact semantics + return value), then **write the mutated slice
  back into the parent's `[offset, offset+size)` range** and resync the view's `size`/
  `expected`. So `subList(1,n).clear()` and friends mutate the parent.

This supersedes the earlier interim states (a detached copy → silent no-op; then a
positional-only view that threw `UnsupportedOperationException` on structural mutation —
"fail loud"). The structural-delegation step closes that gap while keeping the `ASL` class
the ElasticSearch suite already depends on (`ES-FAIL-04`, `subList(...).toArray(T[])`).

> Note: an alternative implementation that returned a genuine `java.util.ArrayList$SubList`
> (commit `9a535e85`, branch `fix/arraylist-sublist-view`) was prototyped and validated, but
> dev's `ASL` is the maintained design (ES depends on the class), so the fix completes `ASL`
> rather than replacing it.

## Verification (fixed binary)

- `AcknowledgementsTest`: **20/20 OK** (was 3 failing).
- `SubListView`/`SubProbe`: `subList(1,4).clear()` → `[0,4,5]`; positional `set` writes
  through; `add`/`remove`/`removeIf` mutate the parent.
- `subList(a,b).toArray(new T[n])` still works (ES `ASL` read path not regressed).

## Blast radius

`subList(a,b).clear()` is the standard range-delete idiom; any code using sublist
mutation was silently corrupted (copy) or hard-failed (positional-only view) on CratonVM.
