> **RETIRED 2026-08-12 — moved out of `docs/known-issues/jdk-only/`.**
>
> An instrument record whose entire deliverable was *"someone run the CratonVM side of the widened probe"*. That was discharged, and then acted on four times over. The widening itself is fully in the tree: all fifteen sections and 540 observables are in `probes/ShadowDifferentialProbe.java` (`dequeEdges` `:1674`, `concurrentAndAtomic` `:1854`, `bigNumbers` `:1983`, `seededRandom` `:2434`, `throwableSurface` `:2503`, dispatched `:272-281`), as are both hazard fixes it asked for (`drainBounded` `:2878`; the unbounded-iterator and unbounded-enumeration caps `:2845-2870`) and its `thrownDetail` discipline (`:2812`). Its seven "families considered and rejected" are argued refusals, each carrying its reason in place.
>
> **Its 540-line HotSpot oracle (lines 319-858) is now STALE and must not be diffed against.** The probe has since gained a declare/manifest ledger (`probes/ShadowDifferentialProbe.java:425`, `declare(...)` `:623-767`); the current transcript is 864 lines with `PROBE-MANIFEST-DIGEST=22732607802c59c2`. Diffing the old transcript manufactures divergence. That is a reason to retire this record, not to keep it.
>
> Successors: W7-32 ran it (96 divergences), then W7-33 / W7-36 / W7-37 / W7-40, and the live figure is in `W7-42-differential-instrument-holes.md` (9).
>
> Previous location: `docs/known-issues/jdk-only/W7-4-differential-probe-widening-round-2.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260812.md`.

# Widening the shadow differential again: fifteen more families, 540 more observables, and the HotSpot oracle to diff them against

**Status: PROBE WIDENED, NOT YET RUN AGAINST CRATONVM.** Written 2026-08-11.
This record claims **no divergence**. It changes one Java file and captures the
HotSpot 25 side of the transcript so that whoever runs the CratonVM side has an
oracle rather than a re-derivation exercise. Everything below about "what a
wrong answer would look like" is a statement about the *contract*, not a
measurement of CratonVM.

## Why round 2 at all

`probes/ShadowDifferentialProbe.java` was widened once, on 2026-08-10, past
`java.util`'s immutable factories. **On its first widened run it found four
whole defect families** — TreeMap's navigable views answering empty,
`Iterator.remove` with no state machine, `String.format`'s float conversions,
and a stream terminal that killed the run. Those are recorded in
W7-1-treemap-views-and-iterator-remove-contract.md.

The interesting number there is not four, it is four-out-of-twelve-sections-on-
the-first-try. A hit rate like that is not a verdict on those twelve families;
it is a verdict on the **sample**. The census counts roughly 1,600 registrations
standing in front of concrete JDK code. Before round 1 the probe reached a few
dozen of them; after round 1 it printed 318 observables; after round 2 it prints
858. That is still not coverage. It is a bigger sample of a surface that has so
far answered "wrong" every time anybody has looked at a new corner of it, which
makes another widening the highest-yield thing available and makes "they match"
continue to mean "the ones anybody looked at match".

## What was added

Fifteen sections, **540 new printed observables** (transcript lines 319–858; the
round-1 lines 1–318 are unchanged). Every family was chosen on one criterion: a
native stands in front of real JDK bytecode, **and a wrong answer there is
quiet** — an empty collection, a stored null where the spec says remove, a
`false`, a no-throw where the spec mandates a throw. None of those announce
themselves, and all of them read as a pass to a caller that only iterates.

| section | what it asks | the quiet wrong answer it would catch |
|---|---|---|
| `mapDefaults` | `merge` / `compute` / `computeIfAbsent` / `computeIfPresent`, `putIfAbsent`, `getOrDefault`, `replace`, `replaceAll`, `remove(k,v)`; `HashMap`'s null key and null values | a null mapping-function result that **stores instead of removing**: `containsKey` says true, `get` says null, and every caller testing `get(k) != null` behaves as if the removal worked. Mirror image: `getOrDefault` over an explicitly-null value must answer the stored null, not the default |
| `mapViewWriteThrough` | `keySet` / `values` / `entrySet` as **views** — remove, `removeIf`, `retainAll`, `clear`, `Iterator.remove`, `Entry.setValue`, and whether a put made *after* the view was handed out shows up in it | a view that is a defensive copy. Reads identically until something removes through it or puts behind it. This is the same defect shape round 1 found in `TreeMap`, asked of the three views every `Map` hands out |
| `subListContracts` | `add`/`clear`/`sort` write-through, nested sub-views, the empty and reversed ranges, and the CME every view op owes after the backing list is structurally modified | a snapshot `subList`: `set` write-through alone (all round 1 asked) passes on a copy that shares the element array but not the size |
| `wrapperViews` | `unmodifiable*`, `checked*`, `synchronized*` — **both** questions: do writes throw, *and* do reads see the backing collection change? | a defensive copy answers the write question correctly and the read question wrongly, and nothing that only writes ever notices. A `checked*` that accepts the wrong type is a guard that is silently not there |
| `navigableEdges` | the accessors on an **empty** navigable map (where the spec splits null-answering from throwing), the inclusive/exclusive bound overloads, null and incomparable-element policies, `firstEntry`'s immutability | maps the **extent** of the already-open W7-1 defect rather than re-reporting it. `firstEntry()` must hand back an immutable snapshot entry while `entrySet()`'s is live — one live, one not, from the same map |
| `dequeEdges` | `ArrayDeque`'s outright null refusal, `push`/`pop` direction, `removeFirst/LastOccurrence`, growth past the initial capacity, `Stack` vs `ArrayDeque` print order, bounded drains | `ArrayDeque` forbids null **because null is its "empty" sentinel** — a shadow that stores one corrupts `poll` for every later caller. `push` goes on the front, and the direction is invisible until something reads the order back |
| `enumCollections` | `EnumMap` ordinal ordering, `EnumSet.of/allOf/noneOf/range/complementOf/copyOf`, `Enum.valueOf`, and **enum constants with bodies** | these key on `ordinal()` and `getDeclaringClass()`, never on hashCode and never on `getClass()` — and a constant with a body has a *different* `getClass()` from its enum type. A shadow reaching for `getClass()` works perfectly until the first constant-specific body |
| `concurrentAndAtomic` | `ConcurrentHashMap`'s null refusal, `CopyOnWriteArrayList`'s snapshot iterator, `ArrayBlockingQueue`'s bounded `offer`, the atomics' CAS and update families | CHM's null refusal is *why* its `get` can be lock-free; accepting one produces a map whose `get` cannot tell absent from present. A bounded queue's `offer` must answer `false` when full rather than growing. COW's iterator must **not** throw and must **not** see concurrent adds — the exact opposite of every other list, so a shared iterator implementation gets one of the two families wrong |
| `bigNumbers` | `BigDecimal` scale vs value, `divide` with no exact quotient, every `RoundingMode` edge, `stripTrailingZeros`, `BigInteger` mod/remainder sign, exact-vs-truncating conversions | `equals` compares scale and `compareTo` does not, so `1.10` and `1.1` are unequal but compare equal; collapsing the two makes a `HashSet<BigDecimal>` silently deduplicate. `divide` with no exact quotient **must throw** rather than pick a precision — the fabricated-success shape of W2-7 |
| `regexSurface` | `Matcher`'s state machine: group positions, the find cursor, `group()` before any match, named and unmatched groups, `appendReplacement`, region, split limits, flags | round 1 reached the engine only through `String.matches`/`replaceAll`, which answer a boolean and a string. The state machine is where it answers quietly — an empty group, a zero offset, a `group()` before a match that returns `""` instead of throwing |
| `formatConversions` | the rest of the `Formatter` conversion table: `%s %S %b %c %,d %(d %x %o %e %g %a`, flags, argument indices, and the five distinct format exceptions | round 1 asked for three float conversions and **two were wrong** (`%e`, `%g`, W7-1). A seam with that hit rate deserves its conversion table, not three more samples |
| `timeSurface` | month-end and leap-day arithmetic, `Period`/`Duration` parse and print, `ChronoUnit`, `DateTimeFormatter` numeric and text patterns | Jan 31 plus one month is Feb 29 in a leap year and Feb 28 otherwise; an implementation that adds 30 days answers something plausible **every time**. `plusMonths(1).minusMonths(1)` is deliberately included: it is not the identity |
| `textFormatting` | `DecimalFormat` patterns and rounding, `NumberFormat` grouping/percent/currency, `MessageFormat` subformats, `SimpleDateFormat` lenient vs strict | `DecimalFormat`'s default rounding is HALF_EVEN, not HALF_UP: a shadow that rounds half-up produces money that is off by a cent in half the cases and right in the other half |
| `seededRandom` | seeded `nextInt/Long/Double/Float/Boolean/Gaussian/Bytes`, the stream forms, `setSeed`, `Collections.shuffle(l, rnd)` | `Random`'s 48-bit LCG is **specified in its javadoc down to the constants**, so a seeded sequence is an exact observable rather than a sample — the only family here where a whole algorithm diffs in one line |
| `throwableSurface` | cause and suppressed-exception mechanics, and the messages **the VM itself mints**: helpful NPE, `/ by zero`, `ClassCastException`, `ArrayStoreException`, `ArrayIndexOutOfBounds`, `NegativeArraySize` | everything else in this probe tests JDK bytecode; these test CratonVM's own exception machinery, which cannot get them right by delegating. The suppressed list is the quiet one — a try-with-resources whose `close` also throws must carry the close failure as **suppressed**, and an empty suppressed array loses the failure entirely with nothing looking wrong |

### Families considered and rejected

* **`java.io` / `java.nio.file`.** Rejected: environment-dependent output (paths,
  temp dirs, separators) would produce false diffs, and the family is
  host-shaped enough that a Windows oracle could not adjudicate a Linux run.
  Also actively being edited elsewhere this session.
* **Named time zones (`ZoneId.of("Europe/…")`).** Rejected: a tzdb version
  difference between the oracle host and the run host is a false divergence.
  Fixed offsets only.
* **`SecureRandom`, `ThreadLocalRandom`, `UUID.randomUUID`, `Math.random`,
  `Instant.now`.** Rejected: not reproducible, so not differentially testable
  this way. Only the *specified* PRNG is in.
* **Identity hash codes and default `toString`.** Rejected for the same reason —
  including `Enum.hashCode`, which is `Object.hashCode`.
* **`%n` in format strings.** Rejected specifically: it expands to the platform
  line separator and would manufacture a divergence between a Windows oracle and
  a Linux run that says nothing at all about the VM.
* **Threading and timing.** Rejected: nondeterministic. Note that nothing in
  `concurrentAndAtomic` blocks — `take()` and the timed `poll` are deliberately
  absent, only `poll()` and `offer()` are used.
* **`Deque`/`NavigableSet` basics, `Arrays`, `Collections`, `Optional`,
  `Comparator`, `BitSet`.** Rejected as already covered by round 1. What was
  added instead is only the part round 1 did not ask: the *refusals* and the
  *bound overloads*.

## The three disciplines, and two pre-existing hazards this fixed

Round 1 learned two of these the hard way, and both mechanisms are used by every
section added here.

1. **Every section is fenced.** A section that throws prints one
   `SECTION-DIED.<name>` line instead of removing every line after it from the
   transcript. That fence is what turned `IntStream.summaryStatistics()` from
   "the probe stops at line 284" into a named defect. A truncated transcript
   reads exactly like a short clean run, which is the specific way the first two
   strict census runs lied.
2. **Nothing relies on an exception, or on a collection shrinking, to
   terminate.** Every drain, every `Matcher.find` loop, every iterate-and-mutate
   carries a guard and reports `...-after-100` rather than hanging.
3. **Print a value, not a verdict** — added here. `probes/JdkOnlyCollectionViewProbe`
   exists because an empty view is the failure mode that reads as a pass
   everywhere a caller only iterates; a line printing `ok` cannot diff, a line
   printing `{d=4, c=3}` can. So every observable prints its actual content, and
   where the *message* is the observable — a `checked*` refusal naming the
   offending type, `Enum.valueOf` naming the constant, a VM-generated helpful
   NPE — a new `thrownDetail` helper prints type **and** message, because the
   existing `thrownBy` reports all of those as a bare class name and would diff
   clean over a wrong message.

Applying discipline 2 to the file turned up **two unbounded-on-failure loops
that were already there**, neither of which looks like a test for an exception:

* `while (!pq.isEmpty()) { drained.append(pq.poll()); }` in `deques`.
  This terminates only if `poll` actually **removes** the head. A poll that
  reads without unlinking spins here forever and truncates every section after
  it. Now `drainBounded`.
* `joinIter` / `joinEnumeration`. A `hasNext` (or `hasMoreElements`) that never
  goes false hangs the whole probe the same way. Both bounded now.

All three guards only ever change the output of a VM that is already wrong, so
the HotSpot transcript is unaffected by them.

## Two sources of false divergence found while capturing the oracle

Both are instrument problems, not VM problems, and both would have shown up as
diff lines that say nothing about CratonVM.

1. **One line depends on the host's default locale.** Round 1's
   `stream.summaryStats` prints `IntSummaryStatistics.toString`, which formats
   its average through `String.format` with the **default** locale. On this host
   it produced `average=3,000000` (comma) and with the locale pinned it produces
   `average=3.000000`. It is the only such line — everything else in the probe,
   round 1 and round 2, pins an explicit `Locale`. **Pin the locale on both
   sides** (`-Duser.language=en -Duser.country=US`) or exclude that one line
   from the diff.
2. **The transcript contains non-ASCII.** Round 1 prints `É` from
   `Character.toUpperCase('é')`; round 2 adds an emoji from
   `String.format("%c", 0x1F600)`. Capture both sides with
   `-Dstdout.encoding=UTF-8`, or the encoding difference swamps the result.

A third, mechanical: if the oracle is captured on Windows and the CratonVM run
happens on Linux, **every** line differs by a trailing CR. Diff with
`--strip-trailing-cr`.

## Reproducing

```sh
# same class files for both sides
javac -d /tmp/probes probes/ShadowDifferentialProbe.java

# oracle
java -Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US \
     -cp /tmp/probes ShadowDifferentialProbe > /tmp/hotspot.txt

# the side that has NOT been run
cratonvm --real-jdk --java-home "$JAVA_HOME" \
     -Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US \
     -cp /tmp/probes ShadowDifferentialProbe \
  | grep -v '^\[cratonvm\]' > /tmp/cratonvm.txt

diff --strip-trailing-cr /tmp/hotspot.txt /tmp/cratonvm.txt
```

Read the result in this order:

1. **`SECTION-DIED.<name>` lines first.** Each one is a family that threw where
   HotSpot did not, and it suppresses every observable below it *in that section
   only*. Fix or triage those before reading anything else, because the section
   they name is unmeasured, not clean.
2. **A missing tail.** The last line of a healthy run is `PROBE-DONE`. If it is
   absent the run died outside a fence — that is a VM-level failure, and a
   transcript that stops early cannot report a difference on any line after it.
3. **`...-after-100` markers.** `no-CME-after-100`,
   `unbounded-iterator-after-100`, `unbounded-find-after-100`,
   `poll-did-not-drain-after-100`. Each means a construct that terminates on
   HotSpot did not terminate here, and the guard is the only reason the rest of
   the transcript exists.
4. Then the ordinary value diffs.

## The HotSpot 25 oracle

Captured 2026-08-11 on Windows, Temurin `25.0.3+9-LTS`, from the class files
built by the `javac` line above, with the locale and encoding pinned as shown.
Three consecutive runs were byte-identical. No section died; `PROBE-DONE` was
reached.

The round-1 portion (lines 1–318) is unchanged from what W7-1 describes, with
the single exception of the `stream.summaryStats` locale note above. What
follows is the **round-2 portion verbatim**, transcript lines 319–858, which is
the part that has never been compared against anything.

```text
Map.getOrDefaultPresent=1
Map.getOrDefaultAbsent=9
Map.putIfAbsentPresent=1
Map.putIfAbsentAbsent=null
Map.afterPutIfAbsent={a=1, b=2}
Map.computeIfAbsentNew=3
Map.computeIfAbsentExisting=1
Map.computeIfAbsentNull=null
Map.computeIfAbsentNullCreatesNoEntry=false
Map.computeIfPresentAbsent=null
Map.computeIfPresentNullRemoves=null
Map.afterComputeIfPresentNull=false
Map.computeOnAbsent=10
Map.computeNullRemoves=null
Map.afterComputeNull=false
Map.mergeAbsentUsesValue=4
Map.mergePresentCombines=9
Map.mergeNullResultRemoves=null
Map.afterMergeNull=false
Map.mergeNullValueThrows=java.lang.NullPointerException
Map.replacePresent=1
Map.replaceAbsent=null
Map.replaceThreeArgMismatch=false
Map.replaceThreeArgMatch=true
Map.removeKeyValueMismatch=false
Map.removeKeyValueMatch=true
Map.replaceAll={a=10, b=20}
Map.forEachOrder=z1;y2;x3;
Map.finalContent={b=2}
HashMap.nullKeyGet=nullKeyValue
HashMap.nullKeyContains=true
HashMap.nullValueGet=null
HashMap.nullValueContainsKey=true
HashMap.getOrDefaultOverNullValue=null
HashMap.containsValueNull=true
HashMap.size=2
HashMap.removeNullKey=nullKeyValue
HashMap.sizeAfterNullKeyRemove=1
keySet.content=[a, b, c]
values.content=[1, 2, 3]
entrySet.content=[a=1, b=2, c=3]
keySet.removeWritesThrough=true:{b=2, c=3}
values.removeWritesThrough=true:{c=3}
keySet.removeIf=true:{c=3, e=5}
entrySet.removeIf=true:{c=3}
keySet.seesLaterPut=true
values.seesLaterPut=true
keySet.sizeAfterLaterPut=2
entrySet.sizeAfterLaterPut=2
keySet.addUnsupported=java.lang.UnsupportedOperationException
values.addUnsupported=java.lang.UnsupportedOperationException
entrySet.setValueWritesThrough={a=100, b=200}
keySet.retainAllWritesThrough=true:{b=2}
values.clearWritesThrough={}:0:true
values.removeRemovesOneMapping=true:[7]:{b=7}
entrySet.iteratorRemoveWritesThrough={b=2}:1
keySet.equalsPlainSet=true
TreeMap.keySetIsSorted=[a, b, c]
subList.content=[b, c, d]
subList.setWritesThrough=[a, B, c, d, e]
subList.addWritesThrough=[a, B, c, d, X, e]
subList.sizeAfterAdd=4
subList.baseSizeAfterAdd=6
subList.removeWritesThrough=true:[a, B, c, d, e]
subList.nestedContent=[c, d]
subList.nestedClearWritesThroughToBase=[a, B, e]
subList.afterNestedClear=[B]
subList.emptyRange=[]
subList.emptyRangeIsEmpty=true
subList.reversedRange=java.lang.IllegalArgumentException
subList.pastEnd=java.lang.IndexOutOfBoundsException
subList.negativeStart=java.lang.IndexOutOfBoundsException
subList.staleSize=java.util.ConcurrentModificationException
subList.staleGet=java.util.ConcurrentModificationException
subList.staleToString=java.util.ConcurrentModificationException
subList.staleIterator=java.util.ConcurrentModificationException
List.of.subList=[1, 2]
List.of.subList.addThrows=java.lang.UnsupportedOperationException
Arrays.asList.subList.setWritesThrough=[a, B, c]:[a, B]
subList.sortWritesThrough=[9, 1, 5, 7, 3]
subList.equalsPlainList=true
unmodifiableList.seesBackingWrite=[a, b, c]
unmodifiableList.sizeAfterBackingWrite=3
unmodifiableList.set=java.lang.UnsupportedOperationException
unmodifiableList.removeIf=java.lang.UnsupportedOperationException
unmodifiableList.sort=java.lang.UnsupportedOperationException
unmodifiableList.replaceAll=java.lang.UnsupportedOperationException
unmodifiableList.iteratorRemove=java.lang.UnsupportedOperationException
unmodifiableList.subListAdd=java.lang.UnsupportedOperationException
unmodifiableList.equalsBacking=true
unmodifiableList.contentAfterAll=[a, b, c]
unmodifiableMap.seesBackingWrite={k=1, k2=2}
unmodifiableMap.getAfterBackingWrite=2
unmodifiableMap.entrySetSetValue=java.lang.UnsupportedOperationException
unmodifiableMap.keySetRemove=java.lang.UnsupportedOperationException
unmodifiableMap.valuesClear=java.lang.UnsupportedOperationException
unmodifiableMap.merge=java.lang.UnsupportedOperationException
unmodifiableMap.computeIfAbsent=java.lang.UnsupportedOperationException
unmodifiableMap.contentAfterAll={k=1, k2=2}
unmodifiableSet.content=[s1, s2]
unmodifiableSet.removeAll=java.lang.UnsupportedOperationException
unmodifiableCollection.content=[c1]
unmodifiableSortedMap.content={a=1, b=2}
unmodifiableList.rewrapIsNewObject=true
checkedList.goodAdd=[fine]
checkedList.badAdd=java.lang.ClassCastException: Attempt to insert class java.lang.Integer element into collection with element type class java.lang.String
checkedList.contentAfterBadAdd=[fine]
checkedList.badSet=java.lang.ClassCastException
checkedMap.badValuePut=java.lang.ClassCastException
checkedMap.badKeyPut=java.lang.ClassCastException
checkedMap.contentAfterBadPuts={}
checkedCollection.badAdd=java.lang.ClassCastException
synchronizedCollection.content=[y, z]
synchronizedMap.content={s=1, t=2}
synchronizedMap.sortedKeys=[s, t]
synchronizedMap.getOrDefault=-1
Collections.emptyMap.get=null
Collections.emptyIterator.hasNext=false
Collections.emptyIterator.next=java.util.NoSuchElementException
Collections.emptySet.equalsEmpty=true
TreeMap.emptyFirstEntry=null
TreeMap.emptyLastEntry=null
TreeMap.emptyFloorKey=null
TreeMap.emptyCeilingEntry=null
TreeMap.emptyPollFirstEntry=null
TreeMap.emptyFirstKeyThrows=java.util.NoSuchElementException
TreeMap.emptyLastKeyThrows=java.util.NoSuchElementException
TreeMap.nullKeyPut=java.lang.NullPointerException
TreeMap.nullKeyGet=java.lang.NullPointerException
TreeSet.emptyFirstThrows=java.util.NoSuchElementException
TreeSet.emptyPollFirst=null
TreeMap.headMapInclusive={a=1, b=2, c=3}
TreeMap.headMapExclusive={a=1, b=2}
TreeMap.tailMapExclusive={c=3, d=4}
TreeMap.subMapBothInclusive={a=1, b=2, c=3}
TreeMap.subMapBothExclusive={b=2}
TreeMap.subMapReversedBounds=java.lang.IllegalArgumentException
TreeMap.floorEntry=b=2
TreeMap.ceilingEntry=c=3
TreeMap.higherEntryAtLast=null
TreeMap.lowerEntryAtFirst=null
TreeMap.navigableKeySet=[a, b, c, d]
TreeMap.descendingKeySetSize=4
TreeMap.firstEntryIsImmutable=java.lang.UnsupportedOperationException
TreeMap.entrySetEntryIsLive={a=9}
TreeMap.descendingWriteThrough={c=3, b=2}:1:{b=2, c=3}
TreeMap.headMapPutOutOfRange=java.lang.IllegalArgumentException
TreeMap.contentAfterAll={a=1, b=2, c=3, d=4}
TreeSet.headSetInclusive=[1, 3, 5]
TreeSet.tailSetExclusive=[5, 7]
TreeSet.subSetInclusiveExclusive=[1, 3]
TreeSet.higherAtLast=null
TreeSet.lowerAtFirst=null
TreeSet.pollLast=7:[1, 3, 5]
TreeSet.descendingWriteThrough=3:[9, 2, 1]:[1, 2, 9]
TreeSet.customComparatorOrder=[c, b, a]
TreeSet.comparatorIsReported=true
TreeSet.customFirst=c
TreeSet.headSetUnderCustomComparator=[c]
TreeSet.nullAddNaturalOrdering=java.lang.NullPointerException
TreeSet.incomparableFirstAdd=java.lang.ClassCastException
TreeMap.comparatorNullForNaturalOrder=null
ArrayDeque.addNull=java.lang.NullPointerException
ArrayDeque.addFirstNull=java.lang.NullPointerException
ArrayDeque.offerNull=java.lang.NullPointerException
ArrayDeque.sizeAfterRefusedNulls=0
ArrayDeque.pushIsAddFirst=[b, a]
ArrayDeque.pop=b:[a]
ArrayDeque.content=[a, x, y, x, z]
ArrayDeque.removeFirstOccurrence=true:[a, y, x, z]
ArrayDeque.removeLastOccurrence=true:[a, y, z]
ArrayDeque.descendingIterator=z;y;a;
ArrayDeque.toArray=[a, y, z]
ArrayDeque.clearThenIsEmpty=0:true:null
ArrayDeque.growsPastInitialCapacity=40:0:39
ArrayDeque.getFirstOnEmpty=java.util.NoSuchElementException
ArrayDeque.popOnEmpty=java.util.NoSuchElementException
LinkedList.acceptsNull=true:1:null
LinkedList.peekOnEmpty=null
LinkedList.removeFirstOnEmpty=java.util.NoSuchElementException
LinkedList.addAtIndex=[a, b, c]:b:2
ArrayDequeAsStack.toString=[2, 1]
Stack.toString=[1, 2]
Stack.peekIsTop=2
Stack.search=2
Stack.popOnEmpty=java.util.EmptyStackException
PriorityQueue.peekIsHead=d
PriorityQueue.customComparatorDrain=d,c,b,a,
PriorityQueue.nullAdd=java.lang.NullPointerException
ConcurrentLinkedQueue.poll=a:[b]
ConcurrentLinkedQueue.nullOffer=java.lang.NullPointerException
ConcurrentLinkedQueue.pollEmpty=null
EnumMap.ordinalOrderRegardlessOfInsertion={RED=1, BLUE=3, VIOLET=4}
EnumMap.keySet=[RED, BLUE, VIOLET]
EnumMap.values=[1, 3, 4]
EnumMap.get=3
EnumMap.getAbsent=null
EnumMap.nullKeyPut=java.lang.NullPointerException
EnumMap.size=3
EnumMap.containsValue=true
EnumMap.equalsPlainHashMap=true
EnumMap.removeThenSize=1:2
EnumSet.ofOrdinalOrder=[RED, VIOLET]
EnumSet.allOf=[RED, GREEN, BLUE, VIOLET]
EnumSet.noneOf=[]
EnumSet.range=[GREEN, BLUE, VIOLET]
EnumSet.complementOf=[GREEN, BLUE, VIOLET]
EnumSet.copyOfCollection=[RED, BLUE]
EnumSet.iterationIsOrdinal=GREEN;VIOLET;
EnumSet.removeReturns=true:false:[BLUE]
EnumSet.containsNonEnum=false
EnumSet.equalsPlainSet=true
EnumSet.retainAll=true:[RED, GREEN]
Enum.valueOf=GREEN
Enum.valueOfBadName=java.lang.IllegalArgumentException: No enum constant ShadowDifferentialProbe.Color.MAUVE
Enum.valueOfNull=java.lang.NullPointerException
Enum.ordinal=2
Enum.name=BLUE
Enum.compareTo=-2
Enum.valuesLength=4
Enum.valuesIsAFreshArray=true
Enum.getDeclaringClass=ShadowDifferentialProbe$Color
Enum.equalsIsIdentity=true
Enum.switchDispatch=g:other
Enum.constantBodyApply=5:6
Enum.constantBodyClassIsEnumClass=false
Enum.constantBodyDeclaringClass=Op
EnumSet.overConstantBodies=[ADD, MUL]
EnumMap.overConstantBodies={MUL=1}
Enum.constantBodyName=MUL:1
CHM.get=1
CHM.nullKeyPut=java.lang.NullPointerException
CHM.nullValuePut=java.lang.NullPointerException
CHM.nullKeyGet=java.lang.NullPointerException
CHM.nullValueMerge=java.lang.NullPointerException
CHM.sizeAfterRefusedNulls=1
CHM.putIfAbsentPresent=1
CHM.computeIfAbsent=2
CHM.computeNullRemoves=null:false
CHM.merge=6
CHM.getOrDefault=-1
CHM.sortedContent={a=6}
CHM.reduceValues=6
CHM.keySetViewAdd=java.lang.UnsupportedOperationException
CHM.newKeySet=[a]:false:1
CHM.searchKeys=found
COW.content=[a, b]
COW.snapshotIteratorDoesNotSeeAdds=ab:4
COW.iteratorRemoveUnsupported=java.lang.UnsupportedOperationException
COW.addIfAbsentDuplicate=false
COW.addIfAbsentNew=true:[a, b, zz]
COW.addAllAbsent=1:[a, b, zz, qq]
ABQ.offerFits=true:true
ABQ.offerWhenFullIsFalse=false
ABQ.sizeAfterRefusedOffer=2
ABQ.remainingCapacity=0
ABQ.addWhenFullThrows=java.lang.IllegalStateException
ABQ.content=[a, b]
ABQ.pollThenOffer=a:true:[b, c]
ABQ.drainTo=2:[a, b]:0
ABQ.nullOffer=java.lang.NullPointerException
ABQ.zeroCapacity=java.lang.IllegalArgumentException
AtomicInteger.getAndIncrement=5
AtomicInteger.incrementAndGet=7
AtomicInteger.compareAndSetMismatch=false:7
AtomicInteger.compareAndSetMatch=true:10
AtomicInteger.getAndUpdate=10:20
AtomicInteger.accumulateAndGet=23
AtomicInteger.getAndSet=23:-1
AtomicInteger.toString=-1
AtomicInteger.intValueOverflow=-2147483648
AtomicLong.addAndGet=1099511627777
AtomicBoolean.compareAndSet=true:false:true
AtomicReference.updateAndGet=a!
AtomicReference.compareAndSetIsIdentity=false:true:z
LongAdder.sum=42:42:42
BigDecimal.toString=1.10
BigDecimal.scale=2:1
BigDecimal.precision=3
BigDecimal.unscaledValue=110
BigDecimal.equalsComparesScale=false
BigDecimal.compareToIgnoresScale=0
BigDecimal.hashCodeTracksScale=false
BigDecimal.setDedupes=2
BigDecimal.add=2.20
BigDecimal.subtract=0.00
BigDecimal.multiplyScaleAdds=1.210
BigDecimal.divideExact=2.5
BigDecimal.divideNonTerminating=java.lang.ArithmeticException
BigDecimal.divideByZero=java.lang.ArithmeticException
BigDecimal.divideRounded=0.33333
BigDecimal.setScaleHalfEven=2
BigDecimal.setScaleHalfEvenOdd=4
BigDecimal.setScaleHalfUp=3
BigDecimal.setScaleFloorNegative=-3
BigDecimal.setScaleUnnecessary=java.lang.ArithmeticException
BigDecimal.stripTrailingZeros=6E+2
BigDecimal.stripThenPlainString=600
BigDecimal.negativeZeroStrip=0
BigDecimal.fromDoubleIsExact=0.1000000000000000055511151231257827021181583404541015625
BigDecimal.valueOfDoubleUsesToString=0.1
BigDecimal.movePointLeft=1.23
BigDecimal.pow=3.375
BigDecimal.intValueExactOnFraction=java.lang.ArithmeticException
BigDecimal.intValueTruncates=1
BigDecimal.toEngineeringString=1E+9
BigDecimal.badString=java.lang.NumberFormatException
BigInteger.pow=1267650600228229401496703205376
BigInteger.modPow=3
BigInteger.gcd=21
BigInteger.toStringRadix=ff
BigInteger.fromRadixString=255
BigInteger.divideByZero=java.lang.ArithmeticException
BigInteger.modNegative=2
BigInteger.remainderNegative=-1
BigInteger.shiftLeft=1180591620717411303424
BigInteger.bitLengthAndCount=8:8
BigInteger.testBit=true
BigInteger.signumNegate=-1:5
BigInteger.longValueExactOverflow=java.lang.ArithmeticException
BigInteger.longValueTruncates=1
BigInteger.badString=java.lang.NumberFormatException
BigInteger.compareTo=1
BigInteger.equalsAcrossConstruction=true
Matcher.groupBeforeFindThrows=java.lang.IllegalStateException
Matcher.startBeforeFindThrows=java.lang.IllegalStateException
Matcher.find1=true:a@b.com:a:b:5:12
Matcher.find2=true:c@d.com:17
Matcher.findExhausted=false
Matcher.groupCount=2
Matcher.groupAfterFailedFind=java.lang.IllegalStateException
Matcher.resetRestartsFind=true:a@b.com
Matcher.groupIndexPastGroupCount=java.lang.IndexOutOfBoundsException
Matcher.findAllBounded=a@b.com;c@d.com;e@f.com;
Matcher.namedGroups=bob/example/4
Matcher.unmatchedOptionalGroupIsNull=a/null/-1/-1
Matcher.matchesVsFind=true:false
Matcher.lookingAt=true:false
Matcher.hitEnd=true/2/5/false
Matcher.replaceAllBackref=baba
Matcher.replaceFirst=Xaa
Matcher.appendReplacement=one dog two dogs
Matcher.quoteReplacement=$1
Matcher.badReplacementRef=java.lang.IndexOutOfBoundsException
Matcher.results=2
Pattern.quoteDefeatsMetachars=false
Pattern.splitLimit2=[a, b,,c]
Pattern.splitDropsTrailingEmpties=[a, b]
Pattern.splitKeepsTrailingEmpties=[a, b, , ]
Pattern.splitZeroWidth=[a, b, c]
Pattern.splitLeadingEmpty=[, a, b]
Pattern.badSyntax=java.util.regex.PatternSyntaxException
Pattern.caseInsensitiveFlag=true
Pattern.dotallFlag=true
Pattern.multilineFlag=true
Pattern.matchesStatic=true
Pattern.asPredicate=true
Pattern.asMatchPredicate=false
Pattern.toStringIsThePattern=a+b
Pattern.backreference=true
Pattern.lookahead=true
Pattern.lookbehind=[a,, b,, c]
Pattern.unicodeClass=true
Pattern.greedyVsReluctant=a><b:a
format.sNull=[null]
format.sUpper=AB
format.sPrecisionTruncates=ab
format.booleanOfNullAndObject=true|false|true
format.charFromCharAndInt=x|A
format.charFromSupplementary=😀
format.grouping=1,234,567
format.parenthesisedNegative=(5)|5
format.zeroPadNegativeFloat=-0003.14
format.zeroPadInt=-0000042|-42     |
format.argumentIndex=a-a-b
format.previousArgument=a-a
format.literalPercent=100%
format.hexAndOctal=FF|0xff|10|010
format.hexOfNegative=ffffffff|ffffffffffffffff
format.scientific=1.23e+04|5e-01|1.230000E-04
format.general=1.23400e-05|1.23e+05|100.000
format.hexFloat=0x1.0p0
format.floatSpecials=NaN|Infinity|-0.0|-Infinity
format.floatRoundingHalfUp=0.3|0.4|1
format.bigDecimalPrecision=2.35|1,234,567.89
format.bigInteger=1180591620717411303424|ff
format.widthOnNull=[      null]
format.plusFlag=+42|+1.50| 42
format.unknownConversion=java.util.UnknownFormatConversionException: Conversion = 'q'
format.missingArgument=java.util.MissingFormatArgumentException
format.wrongArgumentType=java.util.IllegalFormatConversionException
format.illegalFlagCombination=java.util.IllegalFormatFlagsException
format.precisionOnInteger=java.util.IllegalFormatPrecisionException
format.localeUS=1,234.50
format.localeGermany=1.234,50
format.localeFrance=1 234 567
format.formatterAppendable=k=007;1.01
LocalDate.toString=2024-01-31
LocalDate.plusMonthsClampsToShorterMonth=2024-02-29
LocalDate.plusMonthsNonLeapYear=2023-02-28
LocalDate.plusMonthsIsNotThirtyDays=2024-03-01
LocalDate.roundTripIsNotIdentity=2024-01-29
LocalDate.plusDaysAcrossYear=2024-01-01
LocalDate.minusYearsFromLeapDay=2023-02-28
LocalDate.isLeapYear=true:false:true
LocalDate.lengthOfMonth=31:28
LocalDate.dayOfWeek=WEDNESDAY
LocalDate.dayOfYearAfterLeapDay=61
LocalDate.month=JANUARY:1
LocalDate.withDayOfMonthOutOfRange=java.time.DateTimeException
LocalDate.ofBadMonth=java.time.DateTimeException
LocalDate.ofFeb30=java.time.DateTimeException
LocalDate.parseBadDay=java.time.format.DateTimeParseException
LocalDate.parseUnpadded=java.time.format.DateTimeParseException
LocalDate.compareAndEquals=-1:true
LocalDate.until=P1M1D
ChronoUnit.daysBetween=30
ChronoUnit.monthsBetweenIsWhole=0
YearMonth.atEndOfMonth=2024-02-29
Period.parse=P1Y2M3D
Period.normalized=P2Y1M
Period.toTotalMonths=25
Period.parseBad=java.time.format.DateTimeParseException
Duration.parse=PT1H30M10.5S
Duration.toStringOfSeconds=PT1H1M1S
Duration.toStringNegative=PT-1S
Duration.toStringZero=PT0S
Duration.plusAndToMillis=90000
Duration.dividedBy=PT8M34.285714285S
Duration.parseBad=java.time.format.DateTimeParseException
Duration.between=PT1M30S
LocalDateTime.toStringDropsZeroSeconds=2024-01-31T13:05
LocalDateTime.withSeconds=2024-01-31T13:05:07
LocalTime.toStringDropsZeroSeconds=01:02
LocalTime.ofNanoOfDay=00:00:00.000000001
Instant.epoch=1970-01-01T00:00:00Z
Instant.plusNanos=1970-01-01T00:00:00.000000001Z
Instant.toEpochMilli=1500
ZonedDateTime.atFixedOffset=2024-01-31T13:05Z
ZonedDateTime.offsetShift=2024-01-31T15:05+02:00
OffsetDateTime.toInstant=2024-01-31T18:05:00Z
DateTimeFormatter.ISO_DATE=2024-01-31
DateTimeFormatter.ISO_LOCAL_DATE_TIME=2024-01-31T13:05:00
DateTimeFormatter.numericPattern=2024/01/31 13:05:00
DateTimeFormatter.textPattern=Wednesday, January 31, 2024
DateTimeFormatter.shortTextPattern=Wed Jan 31
DateTimeFormatter.twelveHourClock=01:05 PM
DateTimeFormatter.parseRoundTrip=09/03/2024:2024-03-09
DateTimeFormatter.parseWrongPattern=java.time.format.DateTimeParseException
DateTimeFormatter.badPattern=java.lang.IllegalArgumentException
DateTimeFormatter.formatWrongTemporal=java.time.temporal.UnsupportedTemporalTypeException
DecimalFormat.basic=1,234.50
DecimalFormat.negative=-1,234.57
DecimalFormat.doubleTieRounding=2.35:2.35
DecimalFormat.exactTieDefaultIsHalfEven=2.34:2.36
DecimalFormat.exactTieHalfUp=2.35:2.36
DecimalFormat.roundingModeIsReported=HALF_EVEN
DecimalFormat.optionalDigits=1:1
DecimalFormat.percentPattern=12.3%
DecimalFormat.scientificPattern=1.235E4
DecimalFormat.negativeSubpattern=(5.50)
DecimalFormat.groupingSize=3:true
DecimalFormat.toPattern=#,##0.00
DecimalFormat.parse=1234.5
DecimalFormat.parseTrailingGarbage=12
DecimalFormat.parseNotANumber=java.text.ParseException
DecimalFormat.parseIntegerOnly=12
DecimalFormat.formatBigDecimalExactly=12,345.68
DecimalFormat.formatLong=-9,223,372,036,854,775,808.00
NumberFormat.integerInstanceRounds=1,234,568
NumberFormat.percentInstance=76%
NumberFormat.currencyUS=$1,234.50
NumberFormat.currencyNegativeUS=-$1,234.50
NumberFormat.maxFractionDigits=1.00:1.235
NumberFormat.defaultMaxFraction=1.235
MessageFormat.simple=cart has 3 items
MessageFormat.numberSubformat=1
MessageFormat.quotedBrace={0} is x
MessageFormat.choiceSubformat=many
MessageFormat.missingArgument=a {1}
MessageFormat.reorderedIndices=ba
SimpleDateFormat.utc=1970-01-01 00:00:00 UTC:2001-09-09 01:46:40 UTC
SimpleDateFormat.parseRoundTrip=1709208000000
SimpleDateFormat.lenientAcceptsOverflow=2023-03-02
SimpleDateFormat.strictRejectsOverflow=java.text.ParseException
Random.nextIntSequence=-1170105035,234785527,-1360544799
Random.nextIntBoundSequence=30,63,48,84,70,25,
Random.nextIntPowerOfTwoBound=46,3,43,3,19,60,
Random.nextIntOriginBound=10
Random.nextLong=-5025562857975149833
Random.nextDouble=0.7275636800328681
Random.nextFloat=0.7275637
Random.nextBooleanSequence=10100101
Random.nextGaussian=1.1419053154730547
Random.nextBytes=[53, -99, 65, -70, -9, -118, -2, 13]
Random.intsStream=[30, 63, 48, 84, 70]
Random.doublesStreamFirst=[0.7275636800328681, 0.6832234717598454]
Random.setSeedRestartsSequence=-1155869325:-1155869325
Random.sameSeedSameSequence=true
Random.differentSeedsCollide=false
Random.nextIntZeroBound=java.lang.IllegalArgumentException
Random.nextIntNegativeBound=java.lang.IllegalArgumentException
Collections.shuffleSeeded=[b, a, e, f, d, c]
Collections.shuffleSeededTwiceIsStable=true
Throwable.getMessage=wrapper
Throwable.getCauseMessage=root cause
Throwable.toString=java.lang.RuntimeException: wrapper
Throwable.causeToString=java.lang.IllegalStateException: root cause
Throwable.getLocalizedMessage=wrapper
Throwable.noArgMessageIsNull=null
Throwable.noArgToString=java.lang.RuntimeException
Throwable.causeOfNoCauseIsNull=null
Throwable.initCauseAfterCtorThrows=java.lang.IllegalStateException
Throwable.initCauseOnce=java.lang.IllegalArgumentException: c
Throwable.initCauseTwiceThrows=java.lang.IllegalStateException:java.lang.IllegalArgumentException: c
Throwable.selfCauseThrows=java.lang.IllegalArgumentException
Throwable.addSuppressedSelfThrows=java.lang.IllegalArgumentException
Throwable.suppressedFromTryWithResources=body-failed/1/IllegalStateException:close-failed
Throwable.suppressedDefaultIsEmpty=0
Throwable.suppressionDisabled=0:0
Throwable.stackTraceTopFrame=ShadowDifferentialProbe.topFrame
Throwable.stackTraceNonEmpty=true
Throwable.setStackTraceIsHonoured=1:Fake.method(Fake.java:7)
Throwable.customSubclassMessage=java.lang.IllegalArgumentException: custom
VM.nullPointerHelpfulMessage=java.lang.NullPointerException: Cannot invoke "String.length()" because "<local0>" is null
VM.nullFieldAccessMessage=java.lang.NullPointerException: Cannot load from int array because "<local0>" is null
VM.nullArrayStoreMessage=java.lang.NullPointerException: Cannot store to int array because "<local0>" is null
VM.divideByZero=java.lang.ArithmeticException: / by zero
VM.modByZero=java.lang.ArithmeticException: / by zero
VM.longDivideByZero=java.lang.ArithmeticException: / by zero
VM.doubleDivideByZeroIsInfinity=Infinity
VM.classCast=java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Integer (java.lang.String and java.lang.Integer are in module java.base of loader 'bootstrap')
VM.arrayStore=java.lang.ArrayStoreException: java.lang.Integer
VM.arrayIndexOutOfBounds=java.lang.ArrayIndexOutOfBoundsException: Index 5 out of bounds for length 2
VM.negativeArraySize=java.lang.NegativeArraySizeException: -1
VM.arrayLengthOfNull=java.lang.NullPointerException: Cannot read the array length because "<local0>" is null
VM.checkcastToArray=java.lang.ClassCastException: class [I cannot be cast to class [Ljava.lang.String; ([I and [Ljava.lang.String; are in module java.base of loader 'bootstrap')
VM.integerOverflowWraps=-2147483648
VM.intMinValueNegated=-2147483648
VM.intDivideMinByMinusOne=-2147483648
```

## What this does and does not claim

**Does:** the probe now prints 858 observables instead of 318, across fifteen
more families; it compiles under `javac` 25; it is deterministic across repeated
runs; the HotSpot side above is the oracle for all of them; and the three
unbounded-on-failure constructs in the file (one pre-existing drain, two
pre-existing iterator joins) are bounded, so no single broken primitive can
truncate the transcript any more.

**Does not:** **the CratonVM side has not been run.** No binary was built and no
`--real-jdk` run was made in this session, so **no divergence is claimed by this
record and none should be read into it**. Every "the quiet wrong answer it would
catch" cell in the table above is a statement about what the JDK contract
requires, not an observation about CratonVM. If a later session runs the diff,
its findings belong in a new record; this one only says what the instrument now
measures and what the correct answer is.

Nor is this coverage. 858 observables against a census of roughly 1,600
inherited shadows is a larger sample of the same under-sampled surface. The
honest reading of a clean diff remains "the ones anybody looked at match" — it
is just that considerably more of them have now been looked at.
