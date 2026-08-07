# Seven out-of-range list accessors answered `null` instead of throwing

**Status: FIXED 2026-08-06.** Found while closing
[`aotintegration-dev-20260805-segv-and-discovery-RETIRED-20260806.md`](aotintegration-dev-20260805-segv-and-discovery-RETIRED-20260806.md):
`de3c4d35b` made `Collections.unmodifiableList(x).get(i)` stop rejecting valid
indices, and the *invalid* ones then reached the backing's own accessor for the
first time. Several of those do not bounds-check.

Oracle: HotSpot 25, `repro/ListItrEndRepro.java`.

## The rows

| call (2-element list) | HotSpot | before | after |
|---|---|---|---|
| `ArrayList.listIterator(2).next()` | `NoSuchElementException` | `null` | ✅ |
| `ArrayList.listIterator(0).previous()` | `NoSuchElementException` | `null` | ✅ |
| `AbstractSequentialList.get(2)` | `IndexOutOfBoundsException` | `null` | ✅ |
| `CopyOnWriteArrayList.get(2 / 9 / -1)` | `ArrayIndexOutOfBoundsException` | `null` | ✅ |
| `CopyOnWriteArrayList.set(9, x)` | `ArrayIndexOutOfBoundsException` | `null` | ✅ |
| `CopyOnWriteArrayList.remove(9)` | `ArrayIndexOutOfBoundsException` | `null` | ✅ |
| `CopyOnWriteArrayList.add(9, x)` | `IndexOutOfBoundsException` | **returns; list becomes `[a, b, x]`** | ✅ |

`ListItrEndRepro` is now byte-identical to HotSpot.

## Why these two clusters, and why they matter beyond their own classes

**`ArrayList$ListItr`** (`native-builtins/src/lib.rs`,
`native_snapshot_itr_next` / `native_snapshot_list_itr_previous`) returned
`Ok(None)` at each end. Its sibling — `ArrayList$Itr.next()` in
native-collections — already threw, and the `Collections$EmptyIterator` natives
a few lines below the patched ones already carry the comment explaining why a
`null` is worse than the exception. Only the `ListItr` pair had been missed,
which is why `list.iterator()` was correct and `list.listIterator()` was not.

The blast radius is not `ArrayList`. `AbstractSequentialList.get(index)` is

```java
try { return listIterator(index).next(); }
catch (NoSuchElementException exc) { throw new IndexOutOfBoundsException("Index: " + index); }
```

so swallowing the exception made `get(size())` answer `null` on **every**
application list that inherits `get` from `AbstractSequentialList` — directly
and through any `Collections.unmodifiableList` view of it.

**`CopyOnWriteArrayList`** (`native-builtins/src/util_concurrent_ext.rs`) took
`*i as usize` and then tested `idx >= size`, so a negative index wrapped to a
huge `usize` and took the same silent `return null` path as an oversized one.
`add(int, E)` did not test at all: it clamped with `idx.min(size)` and inserted,
so `add(9, "z")` on a two-element list returned normally and left `[a, b, z]`.
That is the shape `026ba86c9` named on `ByteBuffer` — the fabricated value is
worse than the missing exception, because a computation consumes it and carries
on.

## Exception classes are not uniform, and were checked one by one

Measured, not assumed: the absolute accessors (`get`/`set`/`remove`) throw the
`ArrayIndexOutOfBoundsException` **subclass** on `CopyOnWriteArrayList` — it
indexes its `array` field directly — while `add(int, E)` goes through
`rangeCheckForAdd` and throws the **plain** `IndexOutOfBoundsException`, and
`index == size` is legal for `add` and not for the others. See
[`../known-issues/list-out-of-bounds-exception-class-and-message.md`](../known-issues/list-out-of-bounds-exception-class-and-message.md)
for the wider table; one row there is still open (`Vector.get` out of range
answers the plain class where HotSpot answers the subclass) and is untouched
here.

## Regression pin

`vm/tests/unmodifiable_list_get_nonarraylist_backing.rs` — compiles a probe over
nine backing shapes (ArrayList, LinkedList, Vector, `Arrays.asList`,
singleton, `CopyOnWriteArrayList`, a user `AbstractSequentialList`, and two
empty lists), runs it under HotSpot and under CratonVM (default and `--nojit`),
and requires equal values. It pins **both halves** — the reads that must
succeed and the out-of-range reads that must still throw — so a fix that merely
deleted a bounds check fails it.

Checked non-vacuous three ways: it fails on the pre-`de3c4d35b` binary (5 of 7
non-empty backings), it fails on `dev` with `de3c4d35b` but without this fix
(3 rows), and it passes after. The exception *class* is normalised inside the
test so it compares values, not the separately-tracked class divergence.
