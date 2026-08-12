# W7-32 — the round-2 differential, run: 96 divergent observables

**Status: MEASURED 2026-08-12.** First execution of `probes/ShadowDifferentialProbe.java` after its
round-2 widening. HotSpot 25.0.3.9 vs CratonVM `--real-jdk`, one binary (dev at b6f0bca44),
same class files, `-Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US` on both sides.

HotSpot 859 lines, PROBE-DONE. CratonVM 829 lines, PROBE-DONE.
**96 divergent observables. 2 sections died.**

Read the diff in the order the probe's own record prescribes: SECTION-DIED first (that section is
unmeasured, not clean), then a missing PROBE-DONE tail, then `...-after-100` markers, then value diffs.
There are no `-after-100` markers, so no construct that terminates on HotSpot failed to terminate here.

## The two dead sections hide ~41 of the 96

```
SECTION-DIED.dequeEdges=java.util.ConcurrentModificationException
SECTION-DIED.concurrentAndAtomic=java.lang.NullPointerException
```

Every observable after the throw in those two sections is simply absent, so the ArrayDeque (15),
ABQ (10), AtomicInteger (9), ConcurrentLinkedQueue (3), CHM (2) and AtomicReference (2) counts below
are mostly that absence rather than distinct defects. **Fix the two throws first, then re-take the
diff** — the real per-family numbers are not knowable until those sections run to the end.

## Families, by divergent-line count

```
     15 ArrayDeque
     12 format
     10 ABQ
      9 AtomicInteger
      5 Throwable
      4 TreeMap
      4 Stack
      4 LinkedList
      3 subList
      3 VM
      3 TreeSet
      3 PriorityQueue
      3 ConcurrentLinkedQueue
      2 CHM
      2 AtomicReference
      1 values
      1 unmodifiableList
      1 stream
      1 keySet
      1 Random
      1 NumberFormat
      1 Map
      1 LongAdder
      1 HashMap
      1 Enum
      1 COW
      1 AtomicLong
      1 AtomicBoolean
      1 ArrayDequeAsStack
```

## Full diff (HotSpot `<` vs CratonVM `>`)

```diff
289c289
< stream.reuseThrows=java.lang.IllegalStateException
---
> stream.reuseThrows=no-throw
338c338
< Map.mergeNullValueThrows=java.lang.NullPointerException
---
> Map.mergeNullValueThrows=no-throw
352c352
< HashMap.getOrDefaultOverNullValue=null
---
> HashMap.getOrDefaultOverNullValue=DEFAULT
368,369c368,369
< keySet.addUnsupported=java.lang.UnsupportedOperationException
< values.addUnsupported=java.lang.UnsupportedOperationException
---
> keySet.addUnsupported=no-throw
> values.addUnsupported=no-throw
384,385c384,385
< subList.nestedClearWritesThroughToBase=[a, B, e]
< subList.afterNestedClear=[B]
---
> subList.nestedClearWritesThroughToBase=[a, B, c, d, e]
> subList.afterNestedClear=[B, c, d]
398c398
< subList.sortWritesThrough=[9, 1, 5, 7, 3]
---
> subList.sortWritesThrough=[9, 5, 1, 7, 3]
422c422
< unmodifiableList.rewrapIsNewObject=true
---
> unmodifiableList.rewrapIsNewObject=false
446,447c446,447
< TreeMap.nullKeyPut=java.lang.NullPointerException
< TreeMap.nullKeyGet=java.lang.NullPointerException
---
> TreeMap.nullKeyPut=no-throw
> TreeMap.nullKeyGet=no-throw
455c455
< TreeMap.subMapReversedBounds=java.lang.IllegalArgumentException
---
> TreeMap.subMapReversedBounds=no-throw
462c462
< TreeMap.firstEntryIsImmutable=java.lang.UnsupportedOperationException
---
> TreeMap.firstEntryIsImmutable=no-throw
473c473
< TreeSet.descendingWriteThrough=3:[9, 2, 1]:[1, 2, 9]
---
> TreeSet.descendingWriteThrough=3:[9, 2, 1]:[1, 2, 3]
478,479c478,479
< TreeSet.nullAddNaturalOrdering=java.lang.NullPointerException
< TreeSet.incomparableFirstAdd=java.lang.ClassCastException
---
> TreeSet.nullAddNaturalOrdering=no-throw
> TreeSet.incomparableFirstAdd=no-throw
481,510c481,490
< ArrayDeque.addNull=java.lang.NullPointerException
< ArrayDeque.addFirstNull=java.lang.NullPointerException
< ArrayDeque.offerNull=java.lang.NullPointerException
< ArrayDeque.sizeAfterRefusedNulls=0
< ArrayDeque.pushIsAddFirst=[b, a]
< ArrayDeque.pop=b:[a]
< ArrayDeque.content=[a, x, y, x, z]
< ArrayDeque.removeFirstOccurrence=true:[a, y, x, z]
< ArrayDeque.removeLastOccurrence=true:[a, y, z]
< ArrayDeque.descendingIterator=z;y;a;
< ArrayDeque.toArray=[a, y, z]
< ArrayDeque.clearThenIsEmpty=0:true:null
< ArrayDeque.growsPastInitialCapacity=40:0:39
< ArrayDeque.getFirstOnEmpty=java.util.NoSuchElementException
< ArrayDeque.popOnEmpty=java.util.NoSuchElementException
< LinkedList.acceptsNull=true:1:null
< LinkedList.peekOnEmpty=null
< LinkedList.removeFirstOnEmpty=java.util.NoSuchElementException
< LinkedList.addAtIndex=[a, b, c]:b:2
< ArrayDequeAsStack.toString=[2, 1]
< Stack.toString=[1, 2]
< Stack.peekIsTop=2
< Stack.search=2
< Stack.popOnEmpty=java.util.EmptyStackException
< PriorityQueue.peekIsHead=d
< PriorityQueue.customComparatorDrain=d,c,b,a,
< PriorityQueue.nullAdd=java.lang.NullPointerException
< ConcurrentLinkedQueue.poll=a:[b]
< ConcurrentLinkedQueue.nullOffer=java.lang.NullPointerException
< ConcurrentLinkedQueue.pollEmpty=null
---
> ArrayDeque.addNull=no-throw
> ArrayDeque.addFirstNull=no-throw
> ArrayDeque.offerNull=no-throw
> ArrayDeque.sizeAfterRefusedNulls=3
> ArrayDeque.pushIsAddFirst=[b, a, null, null, null]
> ArrayDeque.pop=b:[a, null, null, null]
> ArrayDeque.content=[a, null, null, null, x, y, x, z]
> ArrayDeque.removeFirstOccurrence=true:[a, null, null, null, y, x, z]
> ArrayDeque.removeLastOccurrence=true:[a, null, null, null, y, z]
> SECTION-DIED.dequeEdges=java.util.ConcurrentModificationException
533c513,520
< Enum.valueOfBadName=java.lang.IllegalArgumentException: No enum constant ShadowDifferentialProbe.Color.MAUVE
---
> Enum.valueOfBadName=java.lang.IllegalArgumentException: No enum constant MAUVE
> [SUREFIRE-NPE] Name is null thrown; top Java frames:
> [SUREFIRE-NPE]   #0 ShadowDifferentialProbe.main (ShadowDifferentialProbe.java:237)
> [SUREFIRE-NPE]   #1 ShadowDifferentialProbe.section (ShadowDifferentialProbe.java:2136)
> [SUREFIRE-NPE]   #2 ShadowDifferentialProbe.enumCollections (ShadowDifferentialProbe.java:1248)
> [SUREFIRE-NPE]   #3 ShadowDifferentialProbe.thrownBy (ShadowDifferentialProbe.java:2145)
> [SUREFIRE-NPE]   #4 ShadowDifferentialProbe.lambda$enumCollections$2 (ShadowDifferentialProbe.java:1248)
> [SUREFIRE-NPE]   #5 ShadowDifferentialProbe$Color.valueOf (ShadowDifferentialProbe.java:1293)
561c548
< CHM.reduceValues=6
---
> CHM.reduceValues=null
564c551
< CHM.searchKeys=found
---
> CHM.searchKeys=null
570,594c557
< COW.addAllAbsent=1:[a, b, zz, qq]
< ABQ.offerFits=true:true
< ABQ.offerWhenFullIsFalse=false
< ABQ.sizeAfterRefusedOffer=2
< ABQ.remainingCapacity=0
< ABQ.addWhenFullThrows=java.lang.IllegalStateException
< ABQ.content=[a, b]
< ABQ.pollThenOffer=a:true:[b, c]
< ABQ.drainTo=2:[a, b]:0
< ABQ.nullOffer=java.lang.NullPointerException
< ABQ.zeroCapacity=java.lang.IllegalArgumentException
< AtomicInteger.getAndIncrement=5
< AtomicInteger.incrementAndGet=7
< AtomicInteger.compareAndSetMismatch=false:7
< AtomicInteger.compareAndSetMatch=true:10
< AtomicInteger.getAndUpdate=10:20
< AtomicInteger.accumulateAndGet=23
< AtomicInteger.getAndSet=23:-1
< AtomicInteger.toString=-1
< AtomicInteger.intValueOverflow=-2147483648
< AtomicLong.addAndGet=1099511627777
< AtomicBoolean.compareAndSet=true:false:true
< AtomicReference.updateAndGet=a!
< AtomicReference.compareAndSetIsIdentity=false:true:z
< LongAdder.sum=42:42:42
---
> SECTION-DIED.concurrentAndAtomic=java.lang.NullPointerException
690c653
< format.parenthesisedNegative=(5)|5
---
> format.parenthesisedNegative=-5|5
696c659
< format.hexAndOctal=FF|0xff|10|010
---
> format.hexAndOctal=FF|ff|10|10
702,703c665,666
< format.floatRoundingHalfUp=0.3|0.4|1
< format.bigDecimalPrecision=2.35|1,234,567.89
---
> format.floatRoundingHalfUp=0.3|0.3|1
> format.bigDecimalPrecision=0.00|0.00
707,711c670,674
< format.unknownConversion=java.util.UnknownFormatConversionException: Conversion = 'q'
< format.missingArgument=java.util.MissingFormatArgumentException
< format.wrongArgumentType=java.util.IllegalFormatConversionException
< format.illegalFlagCombination=java.util.IllegalFormatFlagsException
< format.precisionOnInteger=java.util.IllegalFormatPrecisionException
---
> format.unknownConversion=no-throw
> format.missingArgument=no-throw
> format.wrongArgumentType=no-throw
> format.illegalFlagCombination=no-throw
> format.precisionOnInteger=no-throw
713,715c676,678
< format.localeGermany=1.234,50
< format.localeFrance=1 234 567
< format.formatterAppendable=k=007;1.01
---
> format.localeGermany=1,234.50
> format.localeFrance=1,234,567
> format.formatterAppendable=k=007;1.00
791c754
< NumberFormat.currencyNegativeUS=-$1,234.50
---
> NumberFormat.currencyNegativeUS=($1,234.50)
812c775
< Random.nextGaussian=1.1419053154730547
---
> Random.nextGaussian=1.141905315473055
831c794
< Throwable.initCauseAfterCtorThrows=java.lang.IllegalStateException
---
> Throwable.initCauseAfterCtorThrows=no-throw
833,835c796,798
< Throwable.initCauseTwiceThrows=java.lang.IllegalStateException:java.lang.IllegalArgumentException: c
< Throwable.selfCauseThrows=java.lang.IllegalArgumentException
< Throwable.addSuppressedSelfThrows=java.lang.IllegalArgumentException
---
> Throwable.initCauseTwiceThrows=no-throw:java.lang.IllegalArgumentException: c2
> Throwable.selfCauseThrows=no-throw
> Throwable.addSuppressedSelfThrows=no-throw
838c801
< Throwable.suppressionDisabled=0:0
---
> Throwable.suppressionDisabled=1:0
850,851c813,814
< VM.classCast=java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Integer (java.lang.String and java.lang.Integer are in module java.base of loader 'bootstrap')
< VM.arrayStore=java.lang.ArrayStoreException: java.lang.Integer
---
> VM.classCast=java.lang.ClassCastException: java.lang.String cannot be cast to java.lang.Integer
> VM.arrayStore=java.lang.ArrayStoreException: java/lang/Integer
855c818
< VM.checkcastToArray=java.lang.ClassCastException: class [I cannot be cast to class [Ljava.lang.String; ([I and [Ljava.lang.String; are in module java.base of loader 'bootstrap')
---
> VM.checkcastToArray=java.lang.ClassCastException: [I cannot be cast to [Ljava.lang.String;
```
