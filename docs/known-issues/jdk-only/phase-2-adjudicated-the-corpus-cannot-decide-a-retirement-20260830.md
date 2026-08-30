# Phase 2 adjudicated: the corpus cannot decide a retirement, and here is what can

**2026-08-30. L2.** Phase 2 of `HANDOFF-20260828-SCOPE.md` asked for the
adjudication of the `--jdk-only` shadow surface — **1477 native-won triples
across 270 classes** — after seven lanes had fixed the behaviour underneath it.
This is that adjudication.

Its headline is not a list of retirable classes. It is that **the regression
corpus cannot produce that list.** The corpus calls 236 of the 270 classes
retire-safe. Arming all 236 at once fails **54 of 118 corpus vectors** and
breaks **35 of 78 probe families, killing 20 of them outright** — including
families that were byte-identical to HotSpot before the retirement.

---

## 1. What was run

`CRATONVM_ENFORCE_NATIVE_SHADOW=<class>` armed one class at a time over a
14-vector smoke set, 270 classes, ~110 s each, on one pinned binary
(`61140769cd2fe8a2`, `a2ab9e4bb`). Three verdicts, not two, because the
driver's first version printed "the dial fired" for a run that produced zero
reports: RETIRE-SAFE, LOAD-BEARING, UNKNOWN.

The binary is pinned and re-checked every iteration, because the first pass of
this sweep lost 78 rows when `/data/vm-l2s` was republished underneath it at
02:49 — the rows either side of that point are two measurements wearing one
log. The sweep now stops rather than mix.

**Result: 34 LOAD-BEARING, 236 RETIRE-SAFE.** No ABORT; the binary held.

That RETIRE-SAFE column is what this record is about.

---

## 2. Three checks on the green column, each cheaper than the sweep

### Check one — 146 of the 236 greens were never asked anything

The unarmed full-corpus reports name, in `violations[]`, every triple that
actually dispatched. Intersecting the smoke set's reports with the verdicts:

```text
classes the smoke set dispatches a shadowed native on   120 of 270
RETIRE-SAFE verdicts                                    236
  backed by an actual dispatch                           90
  VACUOUS -- nothing on the class was ever called       146
```

**146 greens were the green of a question never posed.** The driver already
carried a guard for this shape one level up — an `UNKNOWN` verdict when a run
produced no REPORTS, written because "zero of nothing is zero". The same error
one level down, zero *dispatches* inside perfectly good reports, went unguarded
and read as the best possible result.

The flag's own doc comment states the asymmetry that makes this fatal: an armed
FAILURE is real; an armed ZERO says only that the corpus asked no question this
scope answers wrongly.

### Check two — a row does not measure the class it names

`EnforceShadowScope::covers` is `class_name.starts_with(prefix)`. The row
labelled `java/io/File` also armed `FileInputStream`, `FileOutputStream` and
`FileDescriptor`; `java/util/HashMap` also armed `$KeyIterator`, `$EntrySet`
and `$Node`. Every row is a claim about a prefix, not about a class.

It follows that **a safe prefix cannot exclude a load-bearing class beneath
it**: no value of the variable arms `java/util/HashMap` but not
`java/util/HashMap$KeyIterator`. That is a fourth difference from a real
retirement, on top of the three the flag documents — **a retirement is
per-TRIPLE; the dial is per-PREFIX.**

(In this particular set it costs nothing: the parents of every load-bearing
inner class are themselves load-bearing, so zero classes had to be dropped.
That is luck, and the next set should re-check rather than inherit it.)

### Check three — it would have re-retired six triples already held back

`retired_shadow.rs` is the actual retirement mechanism: it re-tags a
registration `SyntheticStub`, so `--jdk-only` refuses it and the real class
runs. It holds **eleven triples back deliberately**, reason *"needs-VM-support:
state is not real"*. Six are in the candidate set this sweep produced, on
classes it called RETIRE-SAFE:

```text
java/util/TreeSet.add                          java/util/HashSet.iterator
java/util/ArrayDeque.addLast                   java/util/LinkedList.add
java/util/concurrent/ConcurrentHashMap.put     java/util/Arrays.copyOf
```

---

## 3. Re-asking the hold list, with the instrument that can answer

A hold list is a hypothesis with a date on it — 2026-08-12/17/20 — and seven
lanes had since made those classes' state more real, which is this module's own
stated precondition for retiring a shadow. So it was re-measured rather than
assumed: the families' **content** probes, armed against unarmed on one binary
so every pre-existing red cancels, with `yielded/reached` printed beside each
result so a zero from an unasked dial could not pass as an answer.

Armed with only the hold-list classes the sweep called RETIRE-SAFE:

```text
ArrayListShadowSweep         changed=0    yielded/reached= 5010/5010
ChmShadowSweep               changed=0    yielded/reached=34201/34201
CollectionsShadowSweep       changed=0    yielded/reached= 4035/4035
DequeListShadowSweep         changed=2    yielded/reached= 6479/6479
HashtableVectorShadowSweep   changed=0    yielded/reached= 3710/3710
LinkedSequencedShadowSweep   changed=0    yielded/reached= 3658/3658
MapViewsShadowSweep    DIED 261/302 rows, changed=53
TreeShadowSweep              changed=8    yielded/reached= 4389/4389
UtilTailShadowSweep          changed=0    yielded/reached= 3964/3964
PqOptionalShadowSweep        changed=4    yielded/reached= 3755/3755
```

Every dial fully asked and fully answered — `yielded == reached` in all ten,
`declined_no_bytecode = 0` — so not one of these is a vacuum.

### The defect, attributed to one class

Bisecting `MapViewsShadowSweep` over the seven armed classes, one at a time:

```text
java/util/TreeSet                       rc=0 lines=302  changed=0
java/util/ArrayDeque                    rc=0 lines=302  changed=0
java/util/concurrent/ConcurrentHashMap  rc=1 lines=261  changed=53   <---
java/util/HashSet                       rc=0 lines=302  changed=0
java/util/LinkedList                    rc=0 lines=302  changed=0
java/util/ArrayList                     rc=0 lines=302  changed=0
java/util/Arrays                        rc=0 lines=302  changed=0
```

`ConcurrentHashMap` alone. And the rows it breaks are not ConcurrentHashMap's:

```text
241 Properties keySet                      |[a, b, c]|       ->  |[]|
243 Properties entrySet                    |[a=1, b=2, c=3]| ->  |[]|
251 Properties keySet saw a later put      |[a, b, c, d]|    ->  |[]|
260 Properties keySet.remove wrote through |[b=2, c=3]|      ->  |[]|
    then died, in java/util/concurrent/ConcurrentHashMap$KeyIterator.next
```

JDK 25's `Properties` holds
`private transient volatile ConcurrentHashMap<Object,Object> map` and delegates
its `Hashtable` methods to it. Retiring CHM's natives puts real CHM bytecode
under a map whose state this VM keeps in a side table: every `Properties` view
empties out, and the iterator dies. It is **silent data loss first** — forty
rows of empty views — and a crash only at the row that iterates.

### Three instruments, and only the third could see it

| instrument | verdict on `ConcurrentHashMap` |
| --- | --- |
| 14-vector regression smoke set, armed | **14/14 pass — RETIRE-SAFE** |
| `ChmShadowSweep`, the family's OWN content probe | **0 changed over 39 357 yields** |
| `MapViewsShadowSweep`, another family's probe | **died at 261/302, 53 rows changed** |

The family's own probe being clean is the trap, not the reassurance: it asks
about the operations `ConcurrentHashMap` declares, and a view is another
class's method returning another class's object. **A retirement's blast radius
is its class's USERS.**

Note also where the sweep filed the damage. Its row reads
`java/util/concurrent/ConcurrentHashMap RETIRE-SAFE`; the broken class is
`java/util/Properties`, which the same sweep separately called LOAD-BEARING
with seven failing vectors. Nothing joins those two rows up.

**Guarded in code, not only here.**
`the_held_collection_families_are_not_retired` now carries this measurement in
its doc comment and holds the CHM view and iterator classes explicitly — a
per-class sweep called all five CHM classes RETIRE-SAFE, and an entry there is
the only thing that records they were considered and rejected.

---

## 4. The whole set, against the whole corpus

All 236 RETIRE-SAFE classes armed at once, full corpus, one binary:

```text
armed --jdk-only     REGRESSION SUITE:  64 passed,  54 failed
armed SUITE=all      REGRESSION SUITE: 118 passed,   0 failed
armed SUITE=core     REGRESSION SUITE:  78 passed,   0 failed
```

**Each of those 236 classes passes 14/14 alone. Together they fail 54 of 118.**
Group safety is not the sum of individual safety, and no amount of per-class
sweeping would have said so.

The two clean arms are the control the flag's doc promises — *"no effect
outside `--jdk-only`: the caller tests `is_jdk_only()` first"* — so the 54 are
a strict-mode phenomenon and not a general breakage.

And this run is emphatically not a vacuum:

```text
enforcement_dial   reached 14 123 530   yielded 14 055 769   declined_no_bytecode 67 761
   step1            4 293 059 / 4 286 894      invoke_or_native   1 267 248 / 1 228 185
   cache_revalidate 4 290 893 / 4 277 237      parent_shadow         26 646 /    18 504
   stackless_force  4 232 962 / 4 232 962      cache_populate        10 041 /     9 416
LEAK: armed classes still winning as native -- 0 rows
```

Fourteen million dispatches yielded to real bytecode, every one of the nine
doors asked, and **zero leaks**: the retirement was complete, and the corpus
still fails 54 vectors. The 67 761 `declined_no_bytecode` are triples with no
`Code` to yield to, which run their native either way — an armed run
UNDER-prices those, so 54 is a floor.

### Reach or combination? Four more full-corpus runs say: both

The smoke set was 14 vectors of 118, chosen because 270 classes x a full corpus
is eighteen hours. Running the full corpus for three individual classes shows
what that bought and what it cost:

```text
unarmed baseline                        118 passed,  0 failed
one class: java/util/Locale             118 passed,  0 failed
one class: java/math/BigInteger         116 passed,  2 failed
one class: java/util/concurrent/ConcurrentHashMap
                                        111 passed,  7 failed
all 236 armed together                   64 passed, 54 failed   (reproduced twice)
```

**`ConcurrentHashMap` passes the 14-vector smoke set 14/14 and fails seven
full-corpus vectors.** So the sweep's green column is wrong first because of
its DENOMINATOR — the smoke set made 270 classes affordable and made their
greens meaningless.

But reach is not the whole of it either: 54 is far more than any single class
contributes, and the two clean single-class runs show most classes contribute
nothing alone. The remainder is the mixed heap the flag's contract calls
difference 3 — objects built by a covered class's bytecode meeting objects
built by an uncovered class's native, a state no single-class run reaches.

**A per-class sweep cannot predict a set, at any vector count.**

The failing set is not a corner: `RCollections`, `RJdkViews`, `RJdkNio`,
`RJdkSecurity`, `RJdkForeign`, `RJdkProxy`, `RJdkModule`, `RJdkJmx`,
`RSslLiveSession`, `RClassUnloadSweep` — twelve of them GC- or
class-unloading-shaped, which is where a side table that no longer matches its
object shows up first.

---

## 5. The whole set, against the whole probe tree


The decisive run: all 236 RETIRE-SAFE classes armed at once, 78 probes, three
columns — HotSpot, unarmed `--jdk-only`, armed `--jdk-only`.

Two columns are not enough. `changed` (base vs armed) says a row MOVED and
cannot say which way, and the direction is not always bad. The verdict column
is `d(hs,armed) - d(hs,base)`: **negative means the retirement moves the VM
toward the oracle; only positive is a reason not to retire.**

```text
probes measured                              80
  moved AWAY from HotSpot (delta > 0)        35
    of which one side did not finish         20
  moved TOWARD HotSpot (delta < 0)            5
  unchanged distance                         38
  vacuous (dial never asked)                  2
```

Worst first, and the `d(hs,base)=0` column is the one that matters — those
families were **byte-identical to HotSpot before the retirement**:

```text
BigIntegerSweep         d(hs,base)=0  -> 573   DIED 13072/13255
MapViewsShadowSweep     d(hs,base)=0  -> 287   DIED    21/302
FfmSegmentSweep         d(hs,base)=46 -> 199   DIED     0/199
UriLocaleSweep          d(hs,base)=36 -> 178   DIED   255/403
L4FilesSweep            d(hs,base)=0  -> 121   DIED   278/397
LocaleDateTzShadowSweep d(hs,base)=0  -> 121   DIED     8/125
UtilCoverageSweep       d(hs,base)=0  -> 101   DIED    45/142
PropertiesShadowSweep   d(hs,base)=0  ->  72   DIED   150/184
ForkJoinShadowSweep     d(hs,base)=0  ->  43   DIED   141/178
AsyncChannelSweep       d(hs,base)=0  ->  30   DIED     9/39
```

And the five that got BETTER, which are where a wave should actually look:

```text
AbstractReceiverSweep    d(hs,base)=12 -> 2     delta=-10
L6MsgProbe               d(hs,base)= 8 -> 2     delta= -6
L4Diag                   d(hs,base)= 4 -> 0     delta= -4
DequeListShadowSweep     d(hs,base)= 2 -> 0     delta= -2
PropsOrderSweep          d(hs,base)= 2 -> 0     delta= -2
```

For those five the shadow **was** the defect: `ArrayDeque` armed starts
throwing `ConcurrentModificationException` on modification during iteration,
which is what HotSpot does and what the native never did.

---

## 6. The promotion question, and why it does not apply here

145 of the 1477 triples carry a **second registration** with `owns_slot: false`
— 88 of them on retire-safe classes, across 48 classes, and for several of
those (`JarFile`, `DatagramChannelImpl`, `Runtime$Version`, `EOFException`) on
*every* triple they have. The flag's contract warns that a real retirement promotes that loser
while the dial does not.

**It does not apply to this mechanism, and that is worth writing down once.**
`retired_shadow` re-tags the owner `SyntheticStub` and still calls
`register_inner`, so the slot stays claimed and the loser stays a loser.
Promotion is a hazard of DELETING an `r.register(...)` line, not of adding a
table entry. Anyone retiring by deletion must diff `--dump-native-registry`
across the change; an armed run cannot warn them.

---

## 7. What Phase 2's answer is

**Adjudicated: the surface is 1477 triples over 270 classes; 34 classes are
demonstrably load-bearing; the other 236 are candidates, and not one of them is
retirable on corpus evidence.** The corpus is a filter, not an oracle — it
removes the obviously load-bearing and cannot speak about the remainder,
because it does not ask about the CONTENT of what a retired path built.

A retirement wave needs, per family, all four of:

1. **A dispatch**, proven by `enforcement_dial.reached > 0` for that scope —
   not by a passing vector. 146 of the 236 fail this.
2. **Content probes of the family AND of every family that embeds it**, armed
   against unarmed on one binary, read as a signed distance from HotSpot rather
   than as a changed-row count. 35 of 78 families fail this.
3. **Image bytecode to yield to** — `image_declaring_method` `has_code`,
   declared or inherited and not abstract. Retiring a triple without it trades
   a shadow for an `UnsatisfiedLinkError`.
4. **A dispatch observed in the unarmed corpus**, so the entry rests on a
   measurement rather than on a registration nobody exercises.

Applying 3 and 4 to the sweep's own output, on this binary:

```text
registry distinct triples                        10 151
  on a RETIRE-SAFE class                          3 367
  owner is a Bridge                               3 340   (27 intrinsic, exempt)
  image target has Code to yield to               2 890   (450 would throw)
  and observed shadowing in the unarmed corpus    1 090   <- candidates, 235 classes
```

That 1090 is the *upper* bound, and §5 is the measurement that cuts into it.
Note the funnel needs `--explain-jdk-only`: without it every
`image_declaring_method` is `null` and the "has Code" filter answers **zero for
all 3340 rows** — a field that is false everywhere is a broken reader, not a
fact about the JDK.

## 8. What is fixed here

Nothing is retired, and that is the result. The eleven-triple hold list in
`retired_shadow.rs` is **confirmed correct**, against a sweep that said
otherwise, and extended with the CHM view family and the reason. The next
reader who arms the dial and sees 236 green rows has this page to stop at, and
a test that will fail if they act on them anyway.

The five improving families are the only positive retirement leads this
adjudication produced, and they are worth a wave on their own terms.

## 9. Load-bearing, for whoever takes a family

Classes where arming alone breaks vectors; the failing set names the reason.

```text
java/lang/Class                 8 failed  RStrings RExceptions RReflect RSerial RJdkNio RJdkStrict RJdkReflect RJdkLambdas
java/util/Properties            7 failed  RJdkHello RStrings RSerial RJdkNio RJdkReflect RJdkLambdas RJdkViews
java/util/HashMap$KeyIterator   4 failed  RCollections RJdkNio RConcurrent RJdkViews
java/util/TreeMap$EntryIterator 3 failed  RJdkCollections RJdkReflect RJdkViews
jdk/internal/misc/VM            2 failed  RStrings RJdkNio
java/lang/invoke/MethodHandles  2 failed  RConcurrent RJdkLambdas
```

plus, at one failing vector each: `java/util/TreeMap`, `java/lang/reflect/Field`,
`java/util/TreeMap$EntrySet`, `$KeySet`, `$KeyIterator`,
`java/util/HashMap$EntrySet`, `$EntryIterator`, `$Node`,
`java/lang/reflect/Proxy`, `java/lang/StackTraceElement`,
`java/lang/invoke/LambdaMetafactory`, `sun/nio/ch/SelectionKeyImpl`,
`sun/nio/fs/UnixSecureDirectoryStream`.

Eight of the first fourteen are map view and iterator classes. That is where
the VM's side tables meet the JDK's own view objects, and it is the family a
retirement wave should attempt last rather than first.
