# The last red in the strict arm was a definition-of-done item, not an ordinary defect

**Status: FIXED 2026-08-29.** All three regression arms green. Worktree `h2-known-issues-206dee`, branch
`claude/jdk-only-mode-handoff-09b48c`. Follows the L5 lane; the defect is L3's
by family (`Properties`/`Hashtable`) and was recorded as L3's in
`HANDOFF-20260828-SCOPE.md`.

## 1. What was wrong

```text
cratonvm --jdk-only -cp regression-suite/build RJdkEnumerations

  NoClassDefFoundError: cratonvm/internal/ArrayListViewItr
    at java/util/Collections$SynchronizedCollection.iterator(Collections.java:2328)
    at RJdkEnumerations.sortedImages(RJdkEnumerations.java:131)
    at RJdkEnumerations.properties(RJdkEnumerations.java:162)

  refusing to fabricate a compatibility stand-in for this class
    class="cratonvm/internal/ArrayListViewItr"
    requested_by="native-collections/src/lib.rs:7096"
```

`alloc_arraylist_iterator_as` mints an iterator carrier for an ArrayList-SHAPED
backing. When the backing has no real `modCount` — a `values()` view carrier, a
`Collections$SynchronizedCollection`, a bare placeholder — the carrier is
`cratonvm/internal/ArrayListViewItr`, which no JDK declares. Strict mode refuses
to fabricate it, and the mint site's bare `try_alloc_synthetic(..)?` handed that
refusal straight to the caller.

## 2. Why this one is a definition-of-done item

`HANDOFF-20260828-L7-definition-of-done.md` draws the line the roadmap's
predicate actually screens on:

> **A request is not a failure.** The native asks, is correctly refused, and the
> caller recovers onto real JDK bytecode — that is strict mode working. The
> blocking set is the intersection: *refused AND not recovered from*.

Three of the four fabrication requests that screen found recover. This site did
not, and it is the only vector in the 112-row corpus that was red under
`--jdk-only` and green everywhere else — i.e. **the last remaining MODE defect
in the regression suite**, not merely the last failing test.

## 3. The fix, and the fix that was available and wrong

The recovery already existed one function away. `native_map_key_itr` makes
exactly this move for the refused `HashMap$KeyItr`: catch the refusal, hand back
the same elements through a real `java/util/Arrays$ArrayItr` over an
exact-length snapshot, and record the backing collection with a
`SnapshotItrRoute` so `remove()` still writes through. Four routes were already
wired; this adds `ArrayListView`, whose removal goes to `native_al_remove_obj` —
one call that searches by `equals` through a pinned helper, shifts the tail,
keeps the size/`modCount` bookkeeping, and drops the matching entry from a live
`values()` view's source map.

**The tempting fix is to mint `java/util/ArrayList$Itr` instead, and it is
wrong.** That class NAME is a guarantee that its receiver has a real
`modCount` — it is the precondition that lets `next`/`hasNext` yield to real
bytecode, and it is why the mint was split in two in the first place. Handing it
a `modCount`-less backing re-opens the spurious-`ConcurrentModificationException`
hazard on every `for (v : map.values())`. Both halves of that split carry
comments saying so. The recovery had to be a refusal ARM, not a change to which
class gets minted.

## 4. What the recovery costs, measured

`probes/L3ViewItrSweep.java` — 21 rows, both modes, HotSpot 25.0.3+9 as oracle.
It deliberately includes the rows where a snapshot CANNOT match a live view,
because a fallback whose price is not measured is a fallback whose price is
unknown.

The row that mattered most came back in the VM's favour. HotSpot's own
`Properties.values()` iterator — the Hashtable-backed path this recovery serves
— does **not** throw `ConcurrentModificationException` when the map is modified
mid-iteration, while `LinkedHashMap.values()` does. So on the path the snapshot
fallback actually takes, there is no fail-fast behaviour to lose.

## 5. Verification

```text
probes/L3ViewItrSweep.java   21 rows + the DONE marker, 0 differing
                             lines in BOTH modes

  Including the rows that prove the write-through works rather than
  asserting it:
    props.values.itrRemoveDropsKey = null/0   <- the ENTRY goes, not the value
    props.values.itrRemoveAll      = 0/[]     <- drained through remove()
    props.values.removeBeforeNext  = IllegalStateException
    props.values.removeTwice       = IllegalStateException
  and the row that says the fallback costs nothing here:
    props.values.cmeOnPut          = no-throw  (HotSpot agrees)
    hashmap.values.cmeOnPut        = ConcurrentModificationException
                                              (that path does not take it)

RJdkEnumerations --jdk-only   PASS (70 checks)   <- was NoClassDefFoundError

three arms, `f0d2583a1`             before this session
  strict (--jdk-only)   112 / 112       109 / 112
  all                   112 / 112       109 / 112
  core                   72 /  72        71 /  72
```

**All three arms are fully green, and the strict arm has no known-red vector
left.** That had not happened before: `HANDOFF-20260828-SCOPE.md` told every
lane to expect `RJdkEnumerations` red in two of the three arms. It took two
lanes and two independent causes — L6's CHM values cursor for the compatible
half, and this refusal recovery for the strict half.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp regression-suite/build RJdkEnumerations
cratonvm --java-home "$JDK" --jdk-only -cp probes/out L3ViewItrSweep
```
