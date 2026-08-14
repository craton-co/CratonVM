# A sublist's iterators do not write through — FIXED

**Status:** FIXED (2026-08-14). `probes/SubListBehaviourProbe` now diffs
**empty** against HotSpot JDK 25 on all three arms — `--real-jdk`,
`--jdk-only`, and the default — 90 of 90 lines. The seven rows the open page
listed are the ones that moved; the other 83 were already green and stayed
green.

Measured on Azure host 2 (`/data/toolchain/jdk-25`), CratonVM built from
`fix/jca-sublist-residuals-20260814`.

## What was wrong

| row | HotSpot JDK 25 | CratonVM (before) |
| --- | --- | --- |
| `iter.remove` | `[c\|d]/2 base=[a\|c\|d\|e]/4` | `UnsupportedOperationException: remove` |
| `iter.removeSecond` | `[b\|d]/2 base=[a\|b\|d\|e]/4` | `UnsupportedOperationException: remove` |
| `iter.removeAllViaIterator` | `[]/0 base=[a\|e]/2` | `UnsupportedOperationException: remove` |
| `iter.removeBeforeNext` | `IllegalStateException` | `UnsupportedOperationException: remove` |
| `iter.listIterator.set` | `[B\|c\|d]/3 base=[a\|B\|c\|d\|e]/5` | `[b\|c\|d]/3 base=[a\|b\|c\|d\|e]/5` |
| `iter.listIterator.remove` | `[c\|d]/2 base=[a\|c\|d\|e]/4` | `[b\|c\|d]/3 base=[a\|b\|c\|d\|e]/5` |
| `iter.listIterator.add` | `[b\|X\|c\|d]/4 base=[a\|b\|X\|c\|d\|e]/6` | `[b\|c\|d]/3 base=[a\|b\|c\|d\|e]/5` |

`iterator()` snapshotted the slice and returned a real `java.util.Arrays$ArrayItr`
over the copy; `listIterator()` went through `asl_delegate_snapshot` and
returned a real `java.util.ArrayList$ListItr` over a snapshot `ArrayList`. Both
were iterators over a COPY, on a contract (`List.subList`) that specifies a
live view. `Arrays$ArrayItr` declares no `remove()` at all, so those four rows
reached `Iterator.remove()`'s throwing default; `ArrayList$ListItr` HAS working
mutators and they mutated the copy, so those three reported success and changed
nothing.

## The fix: ONE live iterator, not two mechanisms

The open page prescribed two: a `SnapshotItrRoute::SubListView` write-through
for `Iterator.remove`, plus a position-aware carrier for the `ListIterator`
mutators. One mechanism does both, and the reason is a fact about the JDK the
page did not use: **`ArrayList$SubList.iterator()` IS `listIterator()`**. So a
single live iterator over `(view, cursor, lastRet)` serves both entry points,
every method is `java.util.ArrayList$SubList$1`'s own body rewritten against
this VM's view state, and the eighth row — the one the page listed under
"Cause" rather than in its table — closes as a side effect:

```text
sublist.itr.class   HotSpot  java.util.ArrayList$SubList$1
                    before   java.util.Arrays$ArrayItr
                    after    java.util.ArrayList$SubList$1
```

The route-plus-`removed_count` design the page describes would have worked for
`remove` and could not have worked for `set`/`add`: both need the POSITION the
iterator is at, in the VIEW's coordinates, and a snapshot cursor drifts from
the view's indices the moment anything is removed. `removed_count` exists in
that design to paper over exactly that drift. A live cursor has no drift to
correct — `remove()` is the JDK's own `cursor = lastRet; lastRet = -1`, and
`iter.removeAllViaIterator` (which removes index 0 three times running) is the
row that proves it.

## The carrier is the JDK's own class, and that is the risky half

Wearing `java.util.ArrayList$SubList$1` is what fixes the `getClass()` row, and
it is also the receiver-ownership trap that withdrew the first attempt at the
`ArrayList$SubList` carrier one level up: these natives are handed every
`SubList$1` **java.base's own bytecode** built, which is what `--jdk-only`
produces (there `ArrayList.subList` is refused, java.base runs its own body,
and its iterator is a real one with real state).

`sli_base_checked` is the same width test `asl_base_checked` is, for the same
reason — a JDK-built iterator is exactly `class_num_total_fields` wide
(`cursor`, `lastRet`, `expectedModCount`, `val$index`, `this$0`) and carries
none of this VM's three fields — and `sli_delegate_foreign` runs the receiver's
own bytecode when the answer is "not ours". **The open page named this trap and
said to measure both arms; that is what `--jdk-only` staying byte-identical
across the change is.**

Three details that would each have been a silent wrong answer:

* **`lastRet` is written explicitly to `-1`**, never left to whatever
  `alloc_object` zero-initialises an undeclared slot to. An int-zero read as a
  meaningful `lastRet` makes `remove()` before any `next()` delete element 0
  instead of raising — `iter.removeBeforeNext` is that row, and W7-1's
  `lastRet` was the same hazard.
* **The `IllegalStateException` carries NO message.**
  `RuntimeError::IllegalStateException` always supplies one, so routing through
  it printed `java.lang.IllegalStateException: remove` where HotSpot prints
  `java.lang.IllegalStateException` — `Throwable.toString()` omits the suffix
  only for a null message. `Iterator.remove()`'s throwing DEFAULT does carry
  `"remove"`, which is why the snapshot iterator's version keeps it; this is
  the other exception, from the other class, and it is bare. It was the last
  row to go green and it went green on a one-line change with no behavioural
  content, which is a good reminder that a probe diffing TEXT is diffing the
  message too.
* **The interface-level `hasNext`/`next` route to it deliberately.** Their
  generic fallback (`ctx.invoke(class_name, …)`, guarded against shadow
  recursion) would have reached the same bodies anyway — by accident, and only
  while that fallback keeps working. `is_live_sublist_itr` makes it an explicit
  arm, consulted only after the "slot 0 holds an `Object[]`" probe has already
  missed, so the fast path pays nothing.

An image with no real `java.util.ArrayList$SubList$1` to resolve keeps the
snapshot iterators it has always had: `alloc_sli_view` answers `None` and both
entry points fall back. That is deliberate and NOT a second carrier —
registering this surface on a `cratonvm/internal/*` name would add ten
synthetic stubs and trip `stub_ratchet`, for rows only a synthetic-JDK build
could reach. It is the same trade `register_al_sublist_natives` records for the
twelve `List` methods it puts on the real carrier only.

## Regression evidence

`probes/SubListBehaviourProbe` — empty diff vs HotSpot on `--real-jdk`,
`--jdk-only` and default.

Eight neighbouring collection probes were run on both arms against this build
AND against a binary built from pristine `origin/dev` in a separate worktree:

```text
OK   JdkOnlyCollectionViewProbe            (both arms)
OK   ViewClassProbe                        (both arms)
OK   SnapshotIteratorShapeProbe            (both arms)
OK   StrictIterPrimitivesProbe             (both arms)
OK   MapViewBehaviourProbe                 (both arms)
OK   ListOutOfBoundsProbe                  (both arms)
DIFF ListItrInterfaceProbe                 — byte-identical to the control
DIFF ImmutableCollectionsDifferentialProbe — byte-identical to the control
DIFF UnmodifiableListIteratorJitProbe      — byte-identical to the control
```

The three that differ from HotSpot differ from it identically on the control
binary, so none of them moved. They are `LinkedList$ListItr`'s class identity,
`List.of`/`Set.of` duplicate-and-null rejection, and an
`unmodifiableList`-under-JIT NPE — three pre-existing gaps this change neither
caused nor closed. **Taking the control run is what makes that a measurement
rather than an assumption**, and it cost one worktree and two minutes.

`cargo test -p cratonvm-native-collections --all-targets`: green.

## Repro

```bash
javac -d /tmp/p probes/SubListBehaviourProbe.java
java -cp /tmp/p SubListBehaviourProbe > /tmp/hs.txt 2>/dev/null
for arm in --real-jdk --jdk-only; do
  cratonvm $arm --java-home <jdk25> -cp /tmp/p SubListBehaviourProbe 2>/dev/null \
    | grep -v '^\[cratonvm\]' | diff /tmp/hs.txt -
done
```
