# A sublist's iterators do not write through: `Iterator.remove` throws, `ListIterator.set`/`remove`/`add` do nothing

**Status:** OPEN (2026-08-14). Seven rows, one cause. What is left of
[the sublist carrier-class page](../internal/fixed-suite-bugs/netty/arraylist-sublist-carrier-class-FIXED-20260814.md)
after its own row and six more closed — and, unlike that row, nothing to do with
the carrier: identical before and after it, and present on every build back to
whenever `native_asl_iterator` was written.

Measured by `probes/SubListBehaviourProbe` against HotSpot JDK 25, real JDK,
Azure host 2. `--jdk-only` is **clean** on all seven (there `ArrayList.subList`
runs its own body and hands back a live `ArrayList$SubList$1`); this is a
`--real-jdk` gap only.

| row | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `iter.remove` | `[c\|d]/2 base=[a\|c\|d\|e]/4` | `UnsupportedOperationException: remove` |
| `iter.removeSecond` | `[b\|d]/2 base=[a\|b\|d\|e]/4` | `UnsupportedOperationException: remove` |
| `iter.removeAllViaIterator` | `[]/0 base=[a\|e]/2` | `UnsupportedOperationException: remove` |
| `iter.removeBeforeNext` | `IllegalStateException` | `UnsupportedOperationException: remove` |
| `iter.listIterator.set` | `[B\|c\|d]/3 base=[a\|B\|c\|d\|e]/5` | `[b\|c\|d]/3 base=[a\|b\|c\|d\|e]/5` |
| `iter.listIterator.remove` | `[c\|d]/2 base=[a\|c\|d\|e]/4` | `[b\|c\|d]/3 base=[a\|b\|c\|d\|e]/5` |
| `iter.listIterator.add` | `[b\|X\|c\|d]/4 base=[a\|b\|X\|c\|d\|e]/6` | `[b\|c\|d]/3 base=[a\|b\|c\|d\|e]/5` |

**The three `ListIterator` rows are the dangerous ones.** They report success
and change nothing — a silent wrong answer on a live-view contract, where the
`Iterator.remove` rows at least throw.

## Cause

`native_asl_iterator` snapshots the view's slice into an `Object[]` and returns
a real `java.util.Arrays$ArrayItr` over it; `listIterator()` goes through
`asl_delegate_snapshot`, so it returns a real `ArrayList$ListItr` over a
snapshot `ArrayList`. Both are iterators over a COPY. `Arrays$ArrayItr` declares
no `remove()` at all, so the call reaches `Iterator.remove()`'s throwing default;
`ArrayList$ListItr` has working mutators, and they mutate the copy.

```
sublist.itr.class   HotSpot java.util.ArrayList$SubList$1
                    CratonVM java.util.Arrays$ArrayItr
```

Plain `ArrayList.iterator().remove()` and `listIterator().remove()` both work —
this is the sublist view only.

## The fix, when someone takes it

The machinery already exists and is the same one the `--jdk-only` MXBean fix
used: `snapshot_itr_backing_table` records, per iterator object, the live
collection its `remove()` must write through to, and `native_snapshot_itr_remove`
routes by a `SnapshotItrRoute` the CREATOR chose. So:

* add a `SnapshotItrRoute::SubListView`, and have `native_asl_iterator` call
  `real_snapshot_iterator(ctx, buf, len, Some((view, SubListView)))` instead of
  `make_iterator_from_array`;
* remove **by index**, not by element — a list is positional and may hold
  duplicates, and the iterator knows the slot: `cursor - 1` in the view's
  coordinates, through the view's own `remove(int)`, which already writes
  through (`asl_delegate_mutating`);
* `SnapshotItrBacking` needs a `removed_count` alongside `last_removed_cursor`:
  after the first removal the snapshot's indices and the view's diverge, so the
  index is `cursor - 1 - removed_count`. `iter.removeAllViaIterator` is the row
  that catches getting this wrong.

`ListIterator.set`/`add` need more than a route — they need a position-aware
live iterator, i.e. a carrier class over `(view, cursor, lastRet)` with the
`ListIterator` surface registered on it, which `listIterator()`/`listIterator(int)`
would return in place of the snapshot's. That also fixes the `getClass()` row
above as a side effect.

**Do it with `probes/SubListBehaviourProbe` on BOTH arms.** `--jdk-only` is
green here today and a live-iterator change is exactly the shape that breaks it:
the same receiver-ownership trap the carrier hit, one level down.

## Repro

```bash
javac -d /tmp/p probes/SubListBehaviourProbe.java
java -cp /tmp/p SubListBehaviourProbe > /tmp/hs.txt 2>/dev/null
cratonvm --real-jdk --java-home <jdk25> -cp /tmp/p SubListBehaviourProbe 2>/dev/null \
  | grep -v '^\[cratonvm\]' | diff /tmp/hs.txt - | grep iter
```
