# Bug A — `ArrayList.subList()` returns a copy, not a live view  ✅ FIXED

| | |
|---|---|
| **Severity** | High (silent data corruption; broad blast radius) |
| **Kind** | Wrong result (no exception) |
| **Surfaced by** | `org.apache.kafka.clients.consumer.internals.AcknowledgementsTest` (3 failing tests) |
| **CratonVM** | FAIL · **HotSpot** OK |
| **Status** | **FIXED** on branch `fix/arraylist-sublist-view` (commit `9a535e85`, off dev `b89083d7`) |
| **Recommendation** | Fix (done) — merge to dev |

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
`java.util.ArrayList` (a copy), not `java.util.ArrayList$SubList`, and that **all**
sublist mutations (`clear`, iterator `remove`, `removeAll`, `removeIf`, `remove(int)`,
`set`, `add`) failed to reach the parent, while parent-level ops worked. It is **not**
JIT-specific (fails under `--nojit` too).

## Fix

`native_al_sub_list` now, in real-JDK mode, allocates a genuine
`java/util/ArrayList$SubList` and wires `root=this`, `parent=null`, `offset`, `size`,
`modCount`, returning a true live view; the JDK's own `SubList` bytecode then delegates
reads/writes back to the parent (whose other methods remain native), keeping the shared
`elementData` backing consistent. The class is detected via its `root` field; when absent
(synthetic-jdk mode) the previous copy behaviour is retained as a fallback.

## Verification (fixed binary)

- `AcknowledgementsTest`: **20/20 OK** (was 3 failing).
- `SubProbe`/`SubProbe4`: all sublist mutations write through; `sub.root == parent` true (real view).
- `AckRepro` (real `Acknowledgements`): `acknowledgeTypes()` = `[1]` (matches HotSpot).
- Regression: `ProducerRecordTest` 2/2 OK.

## Blast radius

`subList(a,b).clear()` is the standard range-delete idiom; any code using sublist
mutation was silently corrupted on CratonVM. Worth prioritising the merge.
