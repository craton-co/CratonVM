# Lane 1 — `java.util`, `java.text`, `java.time`

**Status 2026-09-10: RUN. 329 of this lane's 959 §1.4 shadows are retired; the
other 630 are classified, every one of them, with the measurement that decided
it.** The lane's remaining work is seven named VM changes, not more adjudication.

**Scope as measured on `origin/dev` at `7a8b79526`: 959 §1.4 shadows over 96
classes.** Prefixes: `java/util/` (excluding `java/util/concurrent/`, which is
L5's), `java/text/`, `sun/util/`, `java/time/`. The nine-lane split's headline
of 963 was taken on a slightly earlier tree; the difference is bookkeeping, and
§2 says how the number is derived so the next reader can re-take it.

Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first. The method, the four
preconditions and the landing protocol are in
[`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

> **Lane T closed 2026-09-10.** Its throwable-family rows are RETIRED
> (`RETIRED_SHADOW_LT_TRIPLES`, 906 triples over 62 classes), so a triple this
> page defers to lane T is either already retired or classified as blocked —
> check the table before treating it as unowned. Record: [the lane T record](../../internal/jdk-only/lane-t-the-throwable-family-retired-and-the-three-defects-the-arm-had-to-find-first-20260910.md).

## 1. Where the lane stands

| disposition | rows | what decided it |
|---|---:|---|
| **RETIRED** — `RETIRED_SHADOW_L1_TRIPLES` | **329** | two waves, §3 |
| HELD — `TreeMap`/`TreeSet` | 157 | state is `tm_array_table()`, a Rust side table; 9 probes worse armed |
| HELD — `LinkedHashMap` + its four views + iterators | 102 | state is `lhm_overlay()`, a Rust side table; 11 probes worse |
| HELD — `HashMap` + views + iterators + `$Node` | 98 | state IS real (`table` holds real `HashMap$Node`s); 9 probes still worse |
| HELD — `Hashtable` + views + `$Entry` | 79 | 4 probes worse |
| HELD — `java/util/jar/` | 45 | **a vacuous green**: 0 worse over 44 probes until one reached it, then +28 |
| HELD — `Date` / `TimeZone` / `sun/util/calendar/` | 40 | 5 probes worse |
| HELD — `Locale` + `sun/util/locale/` + `sun/util/resources/` + `Currency` | 35 | 3 probes worse, one truncates 125 → 8 |
| HELD — the interface and abstract receivers | 28 | §6 — they are NOT dead, and no per-class trial was run |
| HELD — `HashSet` / `LinkedHashSet` remainder | 13 | lane T holds `register_hashset_natives` |
| HELD — `java/text/` | 12 | the second **vacuous green**: 0 worse, then +11 |
| HELD — `ResourceBundle` + `$Control` | 12 | **armed-clean over 51 probes, red on the trial binary** — §8 |
| EXCLUDED — no dispatch any probe can produce | 9 | §5 |
| | **959** | |

`RETIRED_SHADOW_L1_TRIPLES` in `native-api/src/retired_shadow.rs` carries the
329 with the numbers per family; the tests beside it pin every HELD verdict, so
changing one means changing a test.

## 2. How to re-take the 959, because the headline number will rot

```bash
cratonvm --java-home "$JDK" --jdk-only --explain-jdk-only \
         --dump-native-registry census.json -cp apps/probes/out JdkOnlyCensusLoadProbe
```

Then: rows under the four prefixes, `owns_slot && kind == "bridge"`, bucketed
A/B by `image_declaring_method` exactly as L0 §1 defines it — that is 1,085 —
**minus the 126 produced by a registrar whose LIVE footprint spans another
lane**, which are lane T's under L0 §3 and not this lane's to retire.

Two things about that subtraction, both of which cost a re-measure here:

* **Take the footprint from the `--jdk-only` census, not the `--real-jdk`
  one.** A registrar that also emits `java/util/concurrent/` rows looks
  cross-lane in `--real-jdk` and is L1-only in `--jdk-only`, because Phase 3
  already retired the CHM half. Scoring the live tree against the compatible
  census moves 225 rows into lane T that lane T has nothing left to do about.
* `register_hashset_natives` (`native-collections/src/lib.rs:18821`) loops
  `SET_CLASSES = { HashSet, LinkedHashSet, CopyOnWriteArraySet }`. The third is
  L5's, so the registrar is lane T's whole — **including the 42
  `HashSet`/`LinkedHashSet` rows inside this lane's own prefix**. The 13
  single-class `LinkedHashSet` sequenced rows (`getFirst`, `addLast`,
  `reversed`, …) are L1-own by registrar and still unretirable, because
  retiring the encounter-order surface while lane T's `add`/`remove`/`iterator`
  stay native is a split store in the one direction the class cannot survive.

## 3. What was retired, and the two waves that did it

Both waves ran the same loop: arm one prefix at a time with
`CRATONVM_ENFORCE_NATIVE_SHADOW` against a fixed probe subset, keep the
zero-worse prefixes, then prove them on a TRIAL BINARY against a control built
from the same merged tree. Control `cratonvm-l1-ctl2-20260910`, trial
`cratonvm-l1-trial4-20260910`, both at `b88e4d9fb`, arms run concurrently and
each in its own working directory.

**The acceptance numbers, whole probe tree, two binaries:**

```text
  124 probes measured   0 worse   3 better   0 with an unexplained line move
    NullArgMsgProbe   26 diffs -> 6     (-20)
    L1TailSweep       31 diffs -> 18    (-13), and rc 1 -> 0: it stops dying
    L1Wave1Sweep       2 diffs -> 0     (-2)
  inertness: 321 distinct refusals on this lane's prefixes, ZERO with a
  survivor — so no older registration is still serving a triple the table
  retired, and the wave is not the no-op that looks like a clean result.
```

Two numbers in that block were artefacts before the driver gave each arm its
own directory, and both are worth naming because one of them flattered the
change: `L4Diag2` read `delta +1` with `rc 0->1`, and `L4FilesSweep` read
**`delta -111`** with `rc 1->0`. Both probes build a scratch tree at a
RELATIVE path; two concurrent arms in one directory delete each other's files.
Given separate directories the trial is byte-identical to the control on both.
A negative delta is the result you want, which is why it is the one to
distrust.

**The three corpus arms, both binaries, run STRICTLY SEQUENTIALLY behind a
load gate** (`load < 55 && MemAvailable >= 3G`). That is the opposite of the
rule for the probe A/B and the reason is the instrument: the probe tree
compares exact stdout and does not care about load, while
`regression-suite/run.sh` is a pass/fail harness with per-vector timeouts.
Run two of them at once here and the answer changes — `trial4`'s `all` arm
reported **18 failures at load 112** and **0 at load 53**, every one of the 18
carrying a `harness:` twin, which is the tell.

```text
  ctl2     --jdk-only 132/132   SUITE=all 132/132   SUITE=core 92/92
  trial4   --jdk-only 132/132   SUITE=all 132/132   SUITE=core 92/92

  jdk-only census, union over the 132 vectors:
    native-shadows-bytecode  native-won  1420 -> 1243   (-177)
    synthetic-native-registered          1871 -> 2202   (+331)
    interpreter_shadow_unenforced       12049 -> 10567
```

The ratchet deltas are REPORTED and not re-frozen — lane-0 §4 keeps those
three constants for L0, which re-measures after merge:

```text
  BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT   1883 -> 2227
  BASELINE_SYNTHETIC_STUBS_MANAGEMENT      1894 -> 2238
  BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK   1883 -> 2227
```

The rise IS the retirement: a `Bridge` re-tagged `SyntheticStub` is what makes
`--jdk-only` drop it. The stub LIST diff against the control is **329 added,
every one under this lane's prefixes, none removed** — which is the check the
count alone cannot make.

**Wave 1 — six families whose state is already the real object (271 rows).**
`ArrayList` (with `$SubList`, `$SubList$1`, `$ListItr`, `$Itr` and
`Arrays$ArrayList`), `ArrayDeque`, `LinkedList` (with `$ListItr`),
`Collections` (with the three empty-iterator carriers and `$SetFromMap`),
`Optional` (with `OptionalInt`/`Long`/`Double`), `Arrays`.

**Wave 2 — four families the probe TREE could not ask about (58 rows).**
`java/util/zip/`, `Stack`/`Vector`, `PriorityQueue`, `java/time/`. A fifth,
`ResourceBundle`, was armed-clean and is HELD — §8 is why, and it is the
most transferable thing this lane measured.

Three of the retired rows are §1.4 defects that yielding REPAIRS, and they are
the reason to read the direction rather than the movement:

* `new ZipFile(<a path that does not exist>)` did not throw
  `NoSuchFileException` on the control — it raised
  `internal error: JarFile: cannot open …`, a `MethodCallFailed::InternalError`,
  which is **uncatchable**, so `catch (Throwable)` never runs and the VM exits 1
  mid-program. Retired, the real bytecode throws;
* `CRC32C.update(byte[], off, len)` accepted `len` running past the end of the
  array where HotSpot throws `ArrayIndexOutOfBoundsException`;
* `Collections.emptyList().listIterator().remove()` invented the message
  `Collections.emptyIterator(): remove() before next()`; HotSpot's
  `IllegalStateException` has none.

`NullArgMsgProbe` moved from 26 diffs to 10 across the two waves — the largest
single movement either wave produced, and none of it was in a family's own
probe.

### The four entries that came off the held list, and what moved them

`the_held_collection_families_are_not_retired` named `ArrayDeque.addLast`,
`LinkedList.add(Object)`, `ArrayList$Itr` and `Arrays.copyOf` as
"needs-VM-support: state is not real" / "load-bearing FOR the retirements
above". Three of the four are the same correction — **the state became real
after that list was written**, for Java serialization, and the comment was the
last to know:

* `ll_set` publishes `first`, `last` and `size` to the receiver's own fields
  beside the overlay, and `ll_alloc_node` uses the real `LinkedList$Node` slot
  order (`item`@0, `next`@1, `prev`@2);
* `ad_ensure_capacity` keeps the JDK's own one-spare-slot emptiness invariant
  on the receiver's real `elements`/`head`/`tail`; the fabricated slot 3
  (`size`) is a spare the real class does not declare, so no real body reads it;
* `al_itr_slots` is the same three slots the real `ArrayList$Itr` declares.

`Arrays.copyOf` is the fourth and is different: it was held *because* the
retired ArrayList five depended on it, and this wave retires the dependents and
the dependency together. That is the only configuration that is not a split
store.

## 4. The finding this lane would most like the next lane to have: a retired PRODUCER makes a zero-invocation CONSUMER reachable

Precondition 4 asks for `invocations > 0` per triple in your own instrument's
run. **It is measured on the UNRETIRED binary, and a zero there means "nothing
reaches this today", not "nothing can reach it".**

Wave 1 excluded seven rows on a zero, including
`java/util/Arrays$ArrayList.<init>([Ljava/lang/Object;)V` — `L1Wave1Sweep`
called `Arrays.asList(...)` and the constructor's counter never moved. The
first trial binary said why that reading was wrong:

```text
  ArrayListShadowSweep row 125   Arrays.asList((Object[]) null)
    HotSpot / control            THREW java.lang.NullPointerException
    trial (asList retired,       no-throw
           <init> not)
```

Real `Arrays.asList` is `return new ArrayList<>(a)`; real
`Arrays$ArrayList.<init>` is `a = Objects.requireNonNull(array)`. The
constructor's counter was 0 **because the native `asList` never reached it**.
Retiring the producer is exactly what makes the consumer reachable — and a
half-retired pair is a new defect, not a conservative choice.

Six of the seven came back in on that argument (the constructor, plus the five
`Collections$SetFromMap` accessors behind `Collections.newSetFromMap`). The
seventh, `java/util/Collections.<clinit>()V`, stays out: a `<clinit>` is
reached by class initialisation, which this table cannot change.

**So: before excluding a row on `invocations = 0`, ask what was serving it. If
that row is in your wave, the zero is about to stop being true.**

## 5. Nine rows are excluded for want of a dispatch, and one of them is a live defect

| rows | why |
|---|---|
| `java/util/Collections.<clinit>()V` | no producer to retire (§4) |
| `ResourceBundle` `getLocale` / `getObject` / `keySet` / `getBaseBundleName` | behind a live NPE — see below |
| `ResourceBundle$1` ×3, `ZipFile$1` ×1 | shared-secret accessor shims; no bytecode can name them |

`new PropertyResourceBundle(stream)` fails on this VM under `--jdk-only` with
`NullPointerException: Cannot invoke "java.util.Collection.toArray()" because
"c" is null`. That is why the four `ResourceBundle` instance methods have no
dispatch to observe: no probe can build a bundle to call them on. It is a
defect in its own right and it is this lane's, unfixed —
`apps/probes/L1TailSweep.java`'s `SECTION-DIED.propertyBundle` row is the pin.

## 6. `java/util/stream` is NOT dead, and the earlier reading of it was backwards

The previous version of this page said: 178 eligible rows under
`java/util/stream/`, one with image `Code`, the rest buckets C/E/F —
"abstract or interface registrations that no dispatch door reaches" — and
asked for them to be deleted as cleanup.

**Measured, and the answer is the opposite.** `apps/probes/L1StreamDoorProbe.java`
is 61 rows of ordinary stream use — `filter`/`map`/`collect`/`sorted`,
`IntStream.range`, `Collectors.groupingBy`, the primitive streams — and it is
byte-identical to HotSpot in both modes. The registry dump from that very run,
under `--jdk-only`:

```text
  java/util/stream/*   178 eligible (owns_slot, Bridge)
                        31 with invocations > 0
                        30 of those 31 are bucket C
  Stream.collect  13    Stream.count  5    Stream.distinct  2
  Stream.filter/map/flatMap/limit/max/min/reduce/peek/sorted/iterator/…  1 each
  IntStream.sum  1      LongStream.sum  1      DoubleStream.sum  1
```

The reason is in `native-collections/src/lib.rs`: this VM mints its stream
carrier with `try_alloc_synthetic(ctx, "java/util/stream/Stream", …)`, so the
receiver's class NAME **is** the interface, and a registration on that name is
reached by an ordinary virtual dispatch. That is the per-row rule the
`DeadDoorProbe` note already states — *dead unless something produces a carrier
with that name* — and here something does.

Deleting those 167 registrations would delete this VM's stream implementation.
The same caution applies to the other 28 interface/abstract receivers in this
lane's own count (`Collection`, `List`, `Map`, `Set`, `Spliterator`,
`PrimitiveIterator$Of*`, `AbstractCollection`, `AbstractMap$SimpleEntry`, …):
they are HELD, not dead, and each needs a per-class trial with a probe that can
say which carrier it is talking to.

## 7. The two vacuous greens, and why the sweep alone would have shipped them

`java/util/jar/` and `java/text/` both read **0 worse over 44 probes** in
wave 1's sweep. Both readings were worthless: `enforcement_dial.reached` was 0
in 44 of 44 and 44 of 44 of those runs, because no probe in the 118-probe tree
touches a `JarFile` or a `BreakIterator`. Writing
`apps/probes/L1TailSweep.java` — 137 rows over `Stack`, `Vector`,
`PriorityQueue`, zip, jar, `java.text`, `java.time` and `ResourceBundle` —
turned both red at once:

```text
  scope             worse  L1TailSweep delta   before the probe existed
  java/util/jar/        1   +28  (and truncates)   0 worse, 44/44 vacuous
  java/text/            1   +11  (and truncates)   0 worse, 44/44 vacuous
  java/util/zip/        0    -1  (and UN-truncates) 0 worse, 43/44 vacuous
  java/util/Stack,Vector 0    0                     0 worse, 40/44 vacuous
  java/util/PriorityQueue 0   0  (639 yields)       0 worse, 37/44 vacuous
  java/util/ResourceBundle 0  0  (60/759 yields)    0 worse, 35/44 vacuous  <- and see §8
```

Four of the eight vacuous families survived both the probe and the trial
binary and are retired; two died at the probe and one died at the trial.
**A vacuous green is not a green**, and this is the worked example the
operations page §7 asks for.

## 8. The prefix that was clean over 51 probes and red on the trial binary

`java/util/ResourceBundle` is the row where the DIAL and the TRIAL BINARY
disagreed, and the trial binary is right.

```text
  armed (CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/ResourceBundle)
      51 probes, 0 worse, 1 better; dial engagement y/r = 60/759
  retired (trial binary, same merged tree, control beside it)
      5 probes worse, 10 rows, every one of them the same value:

      TimeZone.getDisplayName()      HotSpot & control  "Eastern Standard Time"
                                     trial              "Coordinated Universal Time"
      LocaleDateTzShadowSweep 98/99  EST / EDT   ->      UTC / UTC
```

This is Phase 3's *"the dial is the wrong instrument here"* from the other
side. `CRATONVM_ENFORCE_NATIVE_SHADOW` declines at nine **dispatch** doors, and
a call that ORIGINATES INSIDE A NATIVE is not one of them.
`TimeZone.getDisplayName` is served by a native that reaches `ResourceBundle`
through `ctx.invoke_*`, so arming the dial left exactly the path the retirement
changes running on the native, and reported clean. A registration REFUSED at
`register` has no native to reach — the real `TimeZoneNameUtility` lookup then
finds no names and falls back to the UTC display name.

**The transferable form: the number of probes an armed sweep was clean over is
not evidence about this failure mode, because the mode is invisible to that
instrument by construction.** Fifty-one clean probes and a well-engaged dial
counter say the same thing as five: candidate, not verdict.

`ResourceBundle` and `$Control` (12 rows) are HELD, and the blocker is named:
the locale-provider lookup the real `getBundle` performs does not find the
JDK's own timezone-name bundles on this VM. That is the same subsystem §9's
locale-provider trap describes, and fixing it is what unblocks these 12 and
probably some of `Locale`'s 35.

## 9. The remedy this lane cannot apply, and one it did

`java/text/Normalizer` is inside a HELD family, so §1.4's remedy — yield to the
bytecode — is not available, and the native has to carry the contract itself.
Five null-argument rows were wrong, measured against HotSpot 25.0.4+7:

```text
                          HotSpot                                    control
  normalize(null, NFC)    NPE …"src" is null                          null
  normalize("a", null)    NPE …"form" is null                         "a"
  normalize(null, null)   NPE …"src" is null   (src wins)             null
  isNormalized(null, NFC) NPE …"src" is null                          true
  isNormalized("a", null) NPE …"form" is null                         true
```

Fixed in `normalizer_reject_nulls`, called from **both** registrars —
`locale_resources.rs` serves the real-JDK boot and
`phases_late/text_intl.rs` serves synthetic mode, and a duplicate pair that
sits half-fixed is exactly what `owns_slot` exists to catch.

## 10. What is left, as six VM changes rather than more measurement

Every remaining HELD family has a named blocker. In rough order of rows:

1. **`TreeMap`/`TreeSet` (157).** `tm_get_slot`/`tm_set_slot` keep the whole map
   in `tm_array_table()`, a Rust `HashMap` keyed by object; the real `root`,
   `size` and `comparator` are never written and no `TreeMap$Entry` node graph
   exists. The remedy is `Properties`' `replace_real_map` for a red-black tree:
   build the real node graph, make it the authority, then retire. Retiring
   first hands real bytecode an empty map.
2. **`LinkedHashMap` + views (102).** `lhm_overlay()`, same shape, same remedy.
3. **`HashMap` + views (98) — the blocker is now ONE METHOD, measured.**
   `apps/probes/L1MapFieldProbe.java` reads, reflectively and under
   `--add-opens=java.base/java.util=ALL-UNNAMED`, the five fields real
   `HashMap$HashIterator` reads, on five receivers built five different ways.
   Armed on `java/util/HashMap` alone, against HotSpot 25.0.4+7:

   ```text
                        table                  size mod thr  load
     HotSpot   A put    [16]HashMap$Node        3    3   12  0.75
     armed     A put    [16]HashMap$Node        3    3   12  0.75   <- exact
     HotSpot   C copy   [4]HashMap$Node         3    3    3  0.75
     armed     C copy   [4]HashMap$Node         3    3    3  0.75   <- exact
   ```

   **Every field matches, on every receiver.** So the 2026-09-10 reading —
   "state IS real but nine probes move" — resolves: the object model is not the
   problem and `modCount`/`threshold`/`loadFactor` are not the answer. What is
   left is one observable:

   ```text
     armed:  keySet.size 3   values.size 3   entrySet.size 3
             keySet walk [a,b,c]   entry walk [a=1,b=2,c=3]   values walk ok
             keySet().toArray()  3      values().toArray()  3
             entrySet().toArray() 0                          <- the blocker
   ```

   The entry ITERATOR works and the entry SIZE is right, so real
   `AbstractCollection.toArray()` should answer 3 and answers 0. Arming
   `AbstractCollection`, `AbstractSet`, `AbstractMap`, `Set`, `Collection` and
   `Map` alongside changes nothing, so it is not one of those registrations
   declining at a door — which, by the §8 argument, points at a native reached
   from inside a native. `toArray` is registered on 34 classes and the
   DISPATCH ROUTE decides the answer; that is where to look next, and the
   probe row to watch is `A.ctor+put.views eToArray`.

   **And the control is worse than the trial on three of these rows**, which
   is worth landing on its own account: unarmed, `new HashMap<>()` + three
   puts leaves `threshold = 0` where HotSpot has 12, `new HashMap<>(64)`
   leaves `threshold = 64` where HotSpot has 48, and **the copy constructor
   and `new HashMap<>(Map.of(..))` leave `table = [16]java.lang.Object` — an
   UNTYPED array where HotSpot has `[4]`/`[2] java.util.HashMap$Node` — with
   `loadFactor = 0.0`.** Retiring the family fixes all five.

   `Hashtable` (79) is NOT the same answer: its fields already match HotSpot
   unarmed, and its views survive arming (`G.hashtable.views` is byte-identical
   in all three columns). Its four moving probes are a separate question.
4. **`java/util/jar/` (45)** and **`java/text/` (12)**: both red only under
   `L1TailSweep`; neither has been bisected to a method. Start by splitting the
   scope — `JarFile` alone, `Manifest`/`Attributes` alone.
5. **`Date`/`TimeZone`/`sun/util/calendar/` (40)** and **`Locale` + providers
   (35)**: read §6 of the previous revision of this page, preserved as the
   locale-provider trap below, before pricing either.
6. **`HashSet`/`LinkedHashSet` (13 + lane T's 42)**: blocked on lane T
   releasing `register_hashset_natives`. Nothing for L1 to do until then.
7. **`ResourceBundle` (12)**: §8. Blocked on the same locale-provider
   lookup as item 5's `Locale`, and it is the cheapest probe into it —
   `TimeZone.getDisplayName` is one call and the answer is one string.

### The locale-provider trap, unchanged and still true

- **`loadInstalled()` answered 0 for every service** and was bypassed *without
  throwing* — every module was in the app loader's catalog, and the probe was
  asking a different lookup than the code used. Probe the same lookup the JDK
  code takes.
- **A blanket null from a shadowing native picks the fallback adapter**, and
  the fallback is root-only on JDK 21 but **not** on 25. Split a composite call
  into its sub-questions before concluding anything about locale data.
- One more, measured by this lane: `ResourceBundle.getBundle("x")` reports
  `locale ` where HotSpot reports `locale en_US`, i.e. the default locale does
  not reach the no-locale overloads. Those three overloads are retired, so this
  is fixed by yielding; the observation is kept because it is evidence about
  which half of the locale subsystem is wrong.

## 11. Standing warnings this lane confirmed

- **A Rust side table is not the object.** §9's items 1 and 2 are the same
  defect `java/util/Properties` had, and the remedy is the same: make the real
  object the authority, THEN drop the native.
- **A stored derived quantity forces you to shadow every mutator.** Retire a
  class's mutators and its derived reads in one wave, or the two disagree.
- **Views and iterators are one unit with their backing class.** Every wave
  here moved `$SubList`, `$ListItr`, `$Itr` with `ArrayList`, and
  `$EmptyIterator`/`$EmptyListIterator`/`$EmptyEnumeration` with `Collections`.
- `toArray` is registered on 34 classes and **the dispatch route decides the
  exception message**, so a probe must print the message, not the outcome.
- `java/util/Hashtable` is still **held** on purpose, and the held-family test
  says so. Amend that test rather than deleting the entry.

## 12. Done

For this lane: every bucket-A/B row in the prefix set is retired, or classified
with the measurement that refused it and the blocker named. That is §1, and it
is complete. What remains is the seven VM changes in §10 — each one a piece of
engineering with a stated acceptance test, not an open question.
