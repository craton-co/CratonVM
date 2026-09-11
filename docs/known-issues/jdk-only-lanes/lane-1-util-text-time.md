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

## 1. Where the lane stands

| disposition | rows | what decided it |
|---|---:|---|
| **RETIRED** — waves 1 and 2, `RETIRED_SHADOW_L1_TRIPLES` | **329** | §3 |
| HELD — `TreeMap`/`TreeSet` | 157 | state is `tm_array_table()`, a Rust side table; 9 probes worse armed |
| HELD — `LinkedHashMap` + its four views + iterators | 102 | state is `lhm_overlay()`, a Rust side table; 11 probes worse |
| PART RETIRED — `HashMap` + views + iterators + `$Node` | 98 | **wave 3**: 21 retired, 77 held. The nine rows that held it were a DIAL ARTEFACT; the twelve probes that hold the 77 are not — §3 |
| HELD — `Hashtable` + views + `$Entry` | 79 | 4 probes worse |
| PART RETIRED — `java/util/jar/` | 45 | **wave 4**: everything but `JarFile`, which is the whole of the `+34` — §3 |
| HELD — `Date` / `TimeZone` / `sun/util/calendar/` | 40 | 5 probes worse; bisected §10 item 6. **Wave 5 wrote a 2-row table for `ZoneInfoFile`, the one non-vacuous `+0`; unaccepted** |
| HELD — `Locale` + `sun/util/locale/` + `sun/util/resources/` + `Currency` | 35 | 3 probes worse, one truncates 125 → 8; bisected §10 item 6. **Wave 5 located the blocker under the five vacuous rows and fixed half of it** — §10 item 6 |
| HELD — the interface and abstract receivers | 28 | §6 — they are NOT dead, and no per-class trial was run |
| HELD — `HashSet` / `LinkedHashSet` remainder | 13 | lane T holds `register_hashset_natives` (`SET_CLASSES` spans `java/util/` and `java/util/concurrent/`). **Wave 5 repaired its one measured row without retiring anything** — §10 item 7 |
| PART RETIRED — `java/text/` | 12 | **wave 4**: `Normalizer` only. `BreakIterator` is the whole `+16`, `DateFormat` is vacuous, `ParseException` repairs nothing and its two moving rows are `Throwable`'s — §3 |
| HELD — `ResourceBundle` + `$Control` | 12 | **armed-clean over 51 probes, red on the trial binary** — §8 |
| EXCLUDED — no dispatch any probe can produce | 9 | §5 |
| | **959** | |

The 959 and its split are the 2026-09-10 census. **Waves 3 and 4 are counted
on their own census (2026-09-11) and are NOT added into that column**, because
two censuses of the same tree are two measurements and forcing them to add up
would invent a number neither took. What they retire, exactly:

```text
  wave 3  RETIRED_SHADOW_L1_HM_TRIPLES   21 triples   java/util/HashMap
  wave 4  RETIRED_SHADOW_L1_JT_TRIPLES   29 triples   java/util/jar/Attributes,
                                                      $Name, JarEntry, Manifest,
                                                      java/text/Normalizer
  wave 5  RETIRED_SHADOW_L1_ZI_TRIPLES    2 triples   sun/util/calendar/ZoneInfoFile
                                                      -- ACCEPTED, see below
```

### Wave 5 — the acceptance

Control `cratonvm-l1w5-base-20260911` is `origin/dev` at `565509592`
untouched; trial `cratonvm-l1w5-trial-20260911` is that same tree plus this
wave. Both built first-attempt on the same host, same toolchain, same day.

```text
  142 probes    0 worse    0 better
    base differing 29    trial differing 29
```

**The baseline is 29, and the first reading was 33.** Four probes —
`L5CasRace`, `L5SubwordAtomics`, `L5UnsafeAccess`, `JcaSunTlsVectors` — read
as differing because HOTSPOT produced zero rows: it died on an
`IllegalAccessError` for want of a runtime `--add-exports`, while this VM,
which does not enforce that module boundary, printed its full output. Every
row then counted as a difference. With the oracle configured like the VM
under test all four go to **0 diff**, and `tree5.sh` now carries the exports
with the 33 → 29 numbers in its comment. Configure the oracle like the thing
you are testing, or you measure your own harness.

**One probe moved, and it is a coin flip in BOTH arms.** `VtHandoffProbe`
read 10 → 16 on the tree. Run alone six times per arm it looked damning —
base `10 10 10 10 10 10`, trial `10 10 0 10 14 10` — which is a different
claim from the one this probe was disarmed on last wave (there the CONTROL
was the unstable one) and not something six unpaired runs can settle. Fourteen
INTERLEAVED pairs, both arms in the same minute:

```text
  BASE : 10 0 10 10 10 10 10 10 10 10 0 14 10 10     3 excursions
  TRIAL: 10 14 10 10 10 10 10 10 10 10 14 10 10 10   2 excursions
```

Same value set `{0, 10, 14}`, and the CONTROL has more excursions than the
trial. The 6/6 stability was luck. The probe's own doc comment says why it
cannot adjudicate this at all: *"on CratonVM it is 64 on an idle host and
55-58 on a loaded one"* — its author measured the row as load-sensitive on
this VM, and this host is never idle.

#### The corpus, and a red arm that is not this wave's

```text
                              control          trial
  jdk-only-strict-probes      FAIL             FAIL
  regression-suite SUITE=all  133 / 133        133 / 133
  regression-suite SUITE=core  93 /  93         93 /  93
```

**Arm 1 fails on `origin/dev` UNTOUCHED**, and the same three probes diverge
in both arms (`JdkOnlyPlatformProbe`, `ChmKeySetGrowth`, `ChmShadowSweep`).
Every "new divergence" is a line of this shape:

```text
  +2026-09-11T21:26:32.831065Z  WARN cas_diag: T19_H6_CAS_DIAG cas_long FAIL #3
     class=java/util/concurrent/ConcurrentHashMap$CounterCell
```

— a timestamped CAS-retry DIAGNOSTIC being compared as if it were program
output. It carries a wall clock, so it can never match a frozen baseline, and
it only appears when a CAS actually contends. **That arm cannot stay green
under load regardless of what any lane does**, and it is worth someone's
attention as a harness defect rather than a VM one.

**The count of those lines nearly cost this wave a false regression, and the
interleaving is what saved it.** Taken sequentially — three control runs, then
two trial runs — the two arms did not overlap at all:

```text
  base  (sequential)  11  9 14  8          range 8-16 combined with the corpus run
  trial (sequential)  15 15 15
```

and there was a MECHANISM ready to explain it, which is the dangerous part:
`try_delegate_real_collection` now runs real bytecode for `size`/`isEmpty` on
carrier receivers, `ConcurrentHashMap$EntrySetView` is a `SET_VIEW_CARRIERS`
entry, and the diagnostics name `ConcurrentHashMap$CounterCell`. A `size()`
that used to answer 0 and now answers correctly makes downstream code do work
it was skipping — more counter updates, more CAS retries, more lines. The
story fits. It is also wrong. Six INTERLEAVED pairs:

```text
  BASE : 12 12 12 15 16 12     range 12-16
  TRIAL: 10 15 16 16 15 12     range 10-16
```

Fully overlapping; the CONTROL reaches 16 and the TRIAL goes down to 10. The
sequential split was the clock, not the binary. **A plausible mechanism is not
evidence, and on this host a sequential A/B is not a measurement** — the same
lesson the probe tree's one mover taught two hours earlier, in a different
instrument.

#### The ratchet

Each arm RUN, none copied from a sibling and none computed as `n + 2`:

```text
  BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT   2547 -> 2549
  BASELINE_SYNTHETIC_STUBS_MANAGEMENT      2558 -> 2560
  BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK   2547 -> 2549
```

`+2` on every arm, and the account the assertion demands is two rows —
`ZoneInfoFile.getZoneInfo` and `getZoneInfo0`, the whole of
`RETIRED_SHADOW_L1_ZI_TRIPLES`. The table's own length and the ratchet are two
instruments reporting one number.

#### The unit gates

```text
  native-api --lib                                 400 passed, 0 failed
  native-collections --lib                         147 passed, 0 failed
  native-builtins --test stub_ratchet               12 passed, 0 failed
  native-builtins --test duplicate_registration_gate 7 passed, 0 failed
  rustfmt --check hunks inside this branch's lines:  0
```

**Wave 5 IS landed, and its table with it.** The paragraph below was written
before the acceptance ran and is kept because the reasoning still stands: the
3,792 engagements bought the table a place in the queue, not a landing. What
landed it is the trial binary.

**Wave 5 was not landed on the strength of its own table.** Its two rows are
the one non-vacuous `+0` in item 6's bisection (3,792 door engagements), and
an armed `+0` is what a LEAKED dial row looks like too — so the 3,792 bought
the table a queue place and nothing more. The trial binary above is what
landed it. Beside the table, wave 5 landed four things that needed no
retirement at all:

```text
  1  try_delegate_real_collection now runs the receiver's own bytecode when
     the receiver's own class declares the method   (§10, "not a family")
  2  map_resize writes HashMap.resize()'s threshold postcondition, which this
     VM never did                                    (§10 item 7's one row)
  3  LocaleResources.getBreakIteratorInfo / getBreakIteratorResources answer
     from the image instead of null                  (§10 items 5, 6, 8)
  4  a guard that NAMES a table missing from RETIRED_SHADOW_TABLES
```

Item 4 started as a red-gate fix and was overtaken mid-session, which is worth
recording as it happened. `triple_is_retired_shadow` consulted ten tables and
`RETIRED_SHADOW_TABLES` listed eight, so
`the_tables_const_lists_every_table_the_predicate_consults` asserted `10 == 8`
and `cargo test -p cratonvm-native-api --lib` was **red on `dev`** — caused by
this lane's own wave-3/4 merge, and verified by reading `origin/dev` at
`059eabf7e` rather than inferred from a branch. The const's doc comment
already carried notes for the same drift at the wave-2 and lane-7 merges;
that made it the third and fourth occurrence, and the first where the
omitting lane was the owning one.

**A sibling fixed it better while this branch was being written**, at
`edc6653d3`: the predicate is now a loop over `RETIRED_SHADOW_TABLES`, so
there is no second list to drift from. The count guard is gone with it. What
this wave keeps is the part the loop does not cover —
`lane_ones_four_tables_are_all_in_the_tables_const`, which NAMES a missing
table. The omission is rarer now and louder: under the loop, a table absent
from the const is not consulted at all, so forgetting one silently
**un-retires a whole wave** rather than merely under-covering it.

All three tables live in `native-api/src/retired_shadow.rs` with the numbers
per family, and the tests beside them pin every HELD verdict — including the
ones waves 3 and 4 had to CHANGE, which is the point of writing them down.

**Wave 3 is the lane's most transferable result, and it cuts twice.**
`java/util/HashMap` sat in the HELD column for two revisions of this page on
nine probe rows that moved when the dial was armed on it. Those nine rows were
a DIAL ARTEFACT — the trial binary reads 0 on all 142 — so **a red dial arm is
a candidate and not a verdict, exactly as a green one is not**. And then the
same trial binary, run against the whole probe tree rather than the one family,
came back **12 worse**: the dial had also been SILENT about two real blockers
it structurally cannot see. Both readings are in §3, and the second is why 77
of the 98 are still held.

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

## 3. What was retired, and the three waves that did it

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


### Wave 3 — `java/util/HashMap` and its views, and the red that was not real

Waves 1 and 2 used the dial to nominate and a trial binary to decide. Wave 3 is
the case where those two disagreed, and the trial binary was right.

**What the dial said.** Armed on `java/util/HashMap`,
`apps/probes/L1MapFamilySweep.java` — 142 rows, written for this wave — moved
nine of them. All nine are the same route:

```text
  ES.toArray        [3]Object[a=1, b=2, c=3]  ->  [0]Object[]
  ES.toArrayObj     [3]Object                 ->  [0]Object
  ES.toArrayEntry   [3]Map$Entry              ->  [0]Map$Entry
  ES.toArrayString  ArrayStoreException       ->  [0]String
  ES.toArrayGen     [3]Object                 ->  [0]Object
  ES.intoArrayList  3                         ->  0
  ES.intoHashSet    3                         ->  0
  X.resize.100      100/9900/100/100          ->  100/9900/100/0
  X.collision.chain 12/12/12                  ->  12/0/12
```

Everything else was exact: `size()`, the entry ITERATOR, `forEach`, `stream`,
`spliterator`, both other views, serialization, comodification, `Node.setValue`
through a detached entry, a 100-entry resize and a 12-way collision chain.

**The row that told us which route answers.** `ES.toArrayString` is
`entrySet().toArray(new String[0])`. Real `AbstractCollection.toArray(T[])`
`aastore`s each element, so for three `Map.Entry`s and a `String[]` it MUST
throw `ArrayStoreException` — and it cannot throw for an empty walk. A quiet
`String[0]` therefore says a native answered and believed the view was empty.
`CRATONVM_DBG_TOARRAY` names it and `CRATONVM_HS_ITR_DBG` corroborates:

```text
  [DBG_TOARRAY] native_al_to_array (0-arg) HIT nargs=1
  [DBG_TOARRAY] al_or_collection_elements recv=java/util/HashMap$EntrySet
                heuristic_len=0 nulls=0 suspect=false
  WARN zgc real: field index OOB index=1 num_slots=1 op="get"   (x10)
  [DBG_TOARRAY] new_ref_array(Object[],len=0) from frame=Mini.main
```

The chain, end to end:

1. the dial's prefix `java/util/HashMap` also covers `$EntrySet`, so
   `entrySet()` yields and hands back the image's OWN `HashMap$EntrySet`.
   `apps/probes/L1EntrySetRouteProbe.java` checks that it really is the JDK's
   object and not a look-alike: `this$0` is the map, `m.entrySet() ==
   m.entrySet()`, and the map's own `entrySet` field is populated — all three
   byte-identical to HotSpot;
2. that object has ONE slot, and `hs_map_slot` puts a view carrier's backing at
   `class_num_total_fields` — slot 1 — because the VM's own mint site allocates
   the carrier WIDER than the class declares. On the image's own object that
   slot is off the end, which is the ten `zgc` warnings, and `hs_backing_map`
   is empty;
3. `toArray` on it resolves up to `java/util/AbstractCollection`, which is not
   armed, so `native_al_to_array` fires;
4. its documented fallback for a layout it does not model is "ask the
   receiver's own `size()`, and walk the real `iterator()` only if it is
   non-zero". **That question is asked from inside a native, where there is no
   dispatch door.** So it reaches `native_hs_size` on `HashMap$EntrySet`, which
   finds no backing and tries `try_delegate_real_collection` — whose
   `invoke_special` re-finds the SAME native, trips its own re-entrancy guard
   and returns the sentinel. `real_size == 0`, no walk, `[0]`.

Java-level `entrySet().size()` answers 3 the whole time, because THAT call
passes a door. The two answers to one question, four frames apart, are the
whole defect.

**What the first trial binary says about those nine rows.** Retirement
does not decline at a door; it removes the registration, so step 4's question
reaches real bytecode. Control `cratonvm-l1hm-base-20260911` (`1d9c00029`,
untouched), trial `cratonvm-l1hm-trial1-20260911` (the same tree plus all 98):

```text
  L1MapFamilySweep  142 rows   control 0 diffs   DIAL-ARMED 9   trial 0
  L1MapFieldProbe              control 5 diffs                  trial 1
  L1EntrySetRouteProbe         control 1 diff                   trial 1
```

So the nine were a dial artefact and the family's own surface is clean. **And
then the whole probe tree said 12 worse, 1 better** — which is the other half
of the lesson, and the more expensive half: the dial was not merely wrong about
the nine, it was SILENT about two blockers it structurally cannot see, because
both of them are about what a SURVIVING native does to an object real bytecode
just built.

```text
  trial1, 127 probes, control cratonvm-l1hm-base-20260911:
    12 worse   1 better
      MethodRefDoorProbe    0 -> 10   rc 0 -> 1   the NPE below
      L4CensusTail          0 -> 51   rc 0 -> 1   truncated by it
      LocaleDateTzShadowSweep 2 -> 29  rc 0 -> 1  truncated by it
      L4FilesSweep          0 -> 18   rc 0 -> 1   truncated by it
      L1TailSweep          13 -> 49               truncated by it
      JcaGapSizer          67 -> 262
      LinkedSequencedShadowSweep 0 -> 8           the overlay below
      SunJceServices        5 -> 9
      SingleByteCharsets    0 -> 4
      JdkOnlyBreadthProbe   0 -> 2
      ItrCarrierCensus      0 -> 1
      L4Reach               0 -> 1
      L1MapFieldProbe       5 -> 1    (better)
```

**Blocker 1 — the three iterator carriers are not this family's to move.**

```text
  NullPointerException: Cannot read field "modCount" because "this.this$0" is null
        at java/util/HashMap$HashIterator.nextNode(HashMap.java:1604)
        at java/util/HashMap$KeyIterator.next(HashMap.java:1628)
```

and the row it killed is `MethodRefDoorProbe`'s **HashSet** row, not its
HashMap row. `key_itr_carrier_for` mints `java/util/HashMap$KeyIterator` for
every receiver that is not LinkedHashMap-shaped — `java/util/HashSet`'s views
and `java/util/Hashtable`'s among them — and those producers are lane T's and
the Hashtable family's, both still `Bridge`. Retire the carrier's natives and
the live producer keeps minting it, so the image's own `HashIterator` bytecode
runs on an object no bytecode built and no constructor filled in. Four more
probes are truncated behind that one throw.

The cluster note on `register_set_view_carrier_natives` predicted this in
prose on 2026-08-20 — *"the four `MAP_KEY_ITR_CARRIERS` cannot be refused while
any `SET_VIEW_CARRIERS` entry outside the moving family is still `Bridge`"* —
and wave 3 is its first measurement. The view CLASSES go with the iterators for
the same reason one class along: retire `HashMap.entrySet()` and real bytecode
mints a real `HashMap$EntrySet` whose own surviving natives then find no
backing, which is the defect this whole section is about, moved sideways.

**Blocker 2 — `LinkedHashMap` inherits eight of these methods.**

```text
  LinkedSequencedShadowSweep
    44 merge counts as an access   {b=22, c=33, a=2}  ->  {a=2}
```

`java/util/LinkedHashMap` is a `HashMap` SUBCLASS. The registry gives it its
own native for 33 of this surface but not for `compute`,
`computeIfPresent`, `equals`, `hashCode`, `merge`, `readObject`, `replaceAll`
or `writeObject` — those dispatch to `java/util/HashMap`'s registration. Retire
them and a LinkedHashMap receiver runs real `HashMap` bytecode over a `table`
its entries are not in: they are in `lhm_overlay()`, which is §10 item 2's
whole problem. The eight are exactly the eight, computed from the registry and
not guessed, and `wave_three_refused_the_iterators_the_views_and_lhm_s_
inherited_eight` is the test that keeps them out.

**What wave 3 therefore retires: 21, not 98.** `java/util/HashMap`'s own map
surface — the four constructors, `put`, `get`, `remove` ×2, `size`, `isEmpty`,
`clear`, `containsKey`, `containsValue`, `putAll`, `putIfAbsent`, `replace`
×2, `computeIfAbsent`, `getOrDefault`, `forEach` and `toString` — and nothing
that produces, carries or is shared with another family's object.

The four `L1MapFieldProbe` rows the wave repairs are unarmed defects the
control has and HotSpot does not: `new HashMap<>()` plus three puts leaves
`threshold = 0` where HotSpot has 12, `new HashMap<>(64)` leaves
`threshold = 64` where HotSpot has 48, and the copy constructor and
`new HashMap<>(Map.of(..))` leave `table = [16]java.lang.Object` — an UNTYPED
array where HotSpot has a typed `HashMap$Node[]` — with `loadFactor = 0.0`.

The two rows that remain are both other people's families and are named here so
nobody re-derives them:

- `F.hashSet.backing.fields` — `new HashSet<>(List.of("a","b","c"))` gives its
  backing map `threshold = 16` where HotSpot has 12. That is
  `java/util/HashSet`'s own constructor, which lane T holds
  (`register_hashset_natives`), and it is §10 item 7;
- `D.treeMap.map.keySetField` — `new TreeMap<>(m)` leaves the map's `keySet`
  field populated where HotSpot leaves it null, in BOTH arms. §10 item 1.

**Precondition 4 is measured, not waived.** Three earlier probe runs reached
29 of the 98 triples. `apps/probes/L1MapFamilySweep.java` — 142 rows — reaches
the rest through ordinary Java: every constructor including the three that
throw, every default-method override on all three views, both iterator
`remove()` contracts, `Node.setValue` through a detached entry, a
serialization round-trip, comodification on each view, a resize and a collision
chain. It is the instrument that measured all three of wave 3's readings, and
it is checked in so the next attempt on the other 77 starts from it.

**One provably-inert repair went in beside the table.** `hs_backing_map` now
bounds the slot against the OBJECT (`object_num_fields`) rather than the class.
The heap already answered a default for an out-of-range slot, so no caller's
answer changes; what stops is the VM reading a cell that is not the object's,
ten times per call, and saying so. It is the same shape as
`has_byte_array_stream_layout` one family over: ask the slot count before
probing the layout.

**One defect is NAMED AND NOT FIXED, on purpose.**
`try_delegate_real_collection` in `native-collections/src/lib.rs` opens with
*"`invoke_special` does an exact per-class native lookup (which finds nothing
for these real classes)"*. That premise is false for every
`SET_VIEW_CARRIERS` entry, because CratonVM registers natives under the real
JDK class NAME — so the helper re-finds itself, trips its guard and returns the
sentinel it exists to avoid. `invoke_special_bytecode_only` is the API for
exactly this and the fix is one line. It is not in this wave because this wave
measured 0 diffs WITHOUT it, and changing a helper that every collection
size/isEmpty native calls is a separate blast radius that deserves its own
control and its own trial. It is §10's own item now, with the trace above as
its evidence.

### Waves 3 and 4 — the acceptance, one binary pair, whole probe tree

Control `cratonvm-l1hm-base-20260911` is `origin/dev` at `1d9c00029`,
untouched. Trial `cratonvm-l1hm-trial2-20260911` is the same tree plus wave
3's 21 triples, wave 4's 29, and one inert bounds check. Arms concurrent, each
in its own working directory, absolute classpath.

```text
  128 probes   0 worse   2 better
    L1JarTextSweep    13 diffs -> 4    (-9)
    L1MapFieldProbe    5 diffs -> 1    (-4)
    VtHandoffProbe     0 diffs -> 5    — NOT a regression, see below
  2 probes time out at 180s on both arms (FjpStress, HibfixComposeProbe2).
```

**`VtHandoffProbe` is a coin flip and this is what that looks like.** Six
sequential runs of each binary, same probe, same host:

```text
  control  5 0 5 5 5 5
  trial    5 7 5 5 0 5
```

Both binaries take every value in {0, 5, 7}. The A/B caught the control on its
one `0` and the trial on a `5`, and reading that pair as a regression would
have cost a day. Measure a flaky vector's floor before explaining it — this
lane has now done that twice on this same probe.

The per-family instruments agree with the tree:

```text
  L1MapFamilySweep  142 rows   control 0   DIAL-ARMED 9   trial 0
  L1JarTextSweep     87 rows   control 13  DIAL-ARMED 4   trial 4
  L1MapFieldProbe               control 5                 trial 1
```

The four `L1JarTextSweep` rows that survive are named in §10 item 5 and in
`RETIRED_SHADOW_L1_JT_TRIPLES`: one is `JarFile`'s, one is the helpful-NPE
-message gap, and two are `Throwable`'s.

**The three corpus arms, both binaries, STRICTLY SEQUENTIAL behind a load
gate** (`load < 55 && MemAvailable >= 4G`) — the opposite of the probe A/B's
rule, for the reason §3 already gives: the probe tree compares exact stdout
and does not care about load, `regression-suite/run.sh` is pass/fail with
per-vector timeouts.

```text
  control  --jdk-only 40/40   SUITE=all 132/132   SUITE=core 92/92
  trial2   --jdk-only 40/40   SUITE=all 132/132   SUITE=core 91/92
```

**`SUITE=core`'s one failure is `RMapGcStress`, and it is the control's.**
Run alone, four times each, under the same gate:

```text
  control  pass pass pass FAIL
  trial2   pass
```

A one-in-four vector inside a 92-vector arm will show up about a fifth of the
time; it did. That is the second flaky instrument this acceptance had to
disarm, after `VtHandoffProbe`, and both were disarmed the same way — run the
CONTROL alone until it disagrees with itself.

```text
  jdk-only census, union over the 40 vectors:
    native-shadows-bytecode  native-won  915 -> 903
    synthetic-native-registered         2202 -> 2254   (+52)
    interpreter_shadow_unenforced       3838 -> 3700
```

The `+52` is the retirement: a `Bridge` re-tagged `SyntheticStub` is what
makes `--jdk-only` drop it. 50 of the 52 are wave 3's 21 and wave 4's 29; the
other two are the same triples counted again under a second feature arm.

### The gate set, on the MERGED tree

`origin/dev` gained lane 5's `RETIRED_SHADOW_L5_TRIPLES` and a re-frozen
`stub_ratchet` between this branch's acceptance and its merge, so the gate set
was run after the merge, not before. The merge is textual and clean, and the
thing to check after it is not that it compiled but that **all nine tables and
all nine chain arms are still there** — a patch authored on a stale base has
`git apply`'d clean over this very file once and silently deleted a sibling
lane's 311-line table. They are, and both of this branch's tables now carry a
disjointness check against lane 5's by name.

```text
  cratonvm-native-api          --lib      378 passed, 0 failed
  cratonvm-native-collections  --lib      144 passed, 0 failed
  cratonvm-types                          607 + 30 passed, 0 failed
  cratonvm-native-builtins     --tests   4238 passed, 0 failed  (lib)
                                         + 45 across the integration binaries
    EXCEPT stub_ratchet::synthetic_stub_count_does_not_regress — RED BY
    DESIGN, and the only red in the set.
```

`registrar_drift::the_drift_baseline_has_no_stale_rows` and the
`flag_inventory` rows, both red for this lane on 2026-09-10 and both traced
then to other lanes, are **green on this tree**. They were fixed where they
belonged.

**The ratchet is re-frozen from the line the test prints, never hand-derived**
— that is the file's own rule, and the 1038 it once froze on was six above
anything any run produced:

```text
  stub-ratchet [no-management]: 2380 SyntheticStub registrations out of 13610
  stub-ratchet: const BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT: usize = 2380;
```

All three arms are re-frozen, each from its own printed line:

```text
  BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT   2332 -> 2384   (+52)
  BASELINE_SYNTHETIC_STUBS_MANAGEMENT      2343 -> 2395   (+52)
  BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK   2332 -> 2384   (+52)
```

Measured twice, because `origin/dev` re-froze these constants underneath this
branch between the two runs — lane 7's wave moved every baseline by `+4`. The
BASELINE moved and this branch's `+52` did not, which is what it means for a
delta to belong to the branch rather than to the tree. The first pair was
`2328/2339 -> 2380/2391`.

and `+52` is the same `+52` the jdk-only census reports for
`synthetic-native-registered` (2202 → 2254). **Two instruments, one number**,
and the arithmetic closes against the tables themselves: 50 distinct triples
plus the two that are registered twice — `java/util/HashMap.<init>()V` and
`java/util/jar/JarEntry.getComment()`. The rise IS the retirement: `register()`
re-tags a retired `Bridge` as `SyntheticStub` so `register_inner` can refuse
it under `--jdk-only`, and this ratchet counts exactly that re-tagging.

Earlier waves of this lane REPORTED the deltas instead of re-freezing, on the
grounds that the constants were lane 0's. That is no longer the convention on
`dev` — lane 5 re-froze them itself on 2026-09-11 — so leaving the gate red
would land a red test, and these are re-frozen.

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

## 10. What is left, as eight VM changes rather than more measurement

Every remaining HELD family has a named blocker. In rough order of rows:

1. **`TreeMap`/`TreeSet` (157).** `tm_get_slot`/`tm_set_slot` keep the whole map
   in `tm_array_table()`, a Rust `HashMap` keyed by object; the real `root`,
   `size` and `comparator` are never written and no `TreeMap$Entry` node graph
   exists. The remedy is `Properties`' `replace_real_map` for a red-black tree:
   build the real node graph, make it the authority, then retire. Retiring
   first hands real bytecode an empty map.
2. **`LinkedHashMap` + views (102).** `lhm_overlay()`, same shape, same
   remedy — and now also a BLOCKER on someone else's table: wave 3 could not
   retire eight `java/util/HashMap` methods because LinkedHashMap inherits
   this VM's registration for them and its entries are in the overlay. Fixing
   item 2 unblocks those eight for free.
3. **`HashMap`'s views, iterators, `$Node` and its three view accessors
   (77).** Wave 3 measured these and put them back; §3 has the two throws.

   **CORRECTED 2026-09-11 (wave 5), and the correction matters for who does
   it.** This item used to say the iterators need "a Hashtable-family
   iterator carrier of its own". That is half the blocker and not the half
   the measurement found. `key_itr_carrier_for`
   (`native-collections/src/lib.rs`) ends in a two-way fallthrough:

   ```rust
     (false, false) => "java/util/HashMap$KeyIterator",
     (false, true)  => "java/util/HashMap$EntryIterator",
   ```

   and `false` there means "not LinkedHashMap-shaped and not CHM" — which
   includes a `java/util/HashSet` receiver. The row that actually broke was
   `MethodRefDoorProbe`'s **HashSet** row, not a Hashtable one. So the set
   needs TWO carriers, not one:

   - `java/util/Hashtable$Enumerator` for the Hashtable family. **L1's**, and
     cheap: `deprecated_util.rs` already builds a REAL one over the live
     table (`HASHTABLE_ENUMERATOR_CLASS`, and its comment "never accept a
     stand-in"), so the carrier exists and only the `key_itr_carrier_for`
     branch is missing.
   - a `HashSet`-family iterator carrier. **NOT L1's.** It has to come with
     `register_hashset_natives`, whose `SET_CLASSES` is `HashSet`,
     `LinkedHashSet` and `java/util/concurrent/CopyOnWriteArraySet` — two
     lanes' prefixes, so lane 0 §3's test makes the registrar lane T's whole.

   Item 7 below was already blocked on that registrar; this item is too, and
   was not recorded as such. Until both carriers exist, any subset of the 77
   is a half-retirement.
4. **`Hashtable` + views + `$Entry` (79).** Four probes worse armed, and
   NOT the same answer as `HashMap` was: its fields already match HotSpot
   unarmed and its views survive arming byte-identically
   (`L1EntrySetRouteProbe`'s `C.hashtable.*` rows are clean in all three
   columns). Its own question, unbisected.
5. **`java/util/jar/JarFile` and `java/text/BreakIterator`.** What is left
   of §7's two vacuous greens after wave 4 took the other six classes: each
   family's whole regression is ONE class.

   **ANSWERED 2026-09-11 (wave 5), and the answer is that neither wants a
   method split.** This item asked for a below-class bisection. Both classes
   turn out to have a single blocker that no subset of their methods escapes,
   and both were readable out of the source rather than measurable out of a
   sweep — which is why no build was spent on the split.

   **`JarFile` (32) is a STATE-MODEL blocker, the same class as items 1 and
   2.** This VM's `JarFile` is a compact synthetic object that does not carry
   `ZipFile`'s state, and the force-native bridge in
   `vm/src/runtime/interpreter/invoke.rs` says so in as many words — "its
   ZipFile state is not present on CratonVM's compact native JarFile
   representation", and for the `super.close()` bridge beside it, "its `res`
   field is absent on CratonVM-native JarFiles". Retire any constructor and
   real `ZipFile` bytecode gets an object with nothing to read, which is
   precisely the `NullPointerException` on `new JarFile(f)` that the +34 is.
   The remedy is `Properties`' `replace_real_map` shape again: build the real
   `ZipFile` state, make it the authority, then retire. Not a method split.

   **And it has TWO PRODUCERS. Wave 5 measured which one wins** rather than
   leaving the next reader a grep — `the_jarfile_constructor_has_two_producers
   _and_this_says_which_wins` in `native-builtins/tests/
   duplicate_registration_gate.rs`, replaying `vm_init`'s real-JDK arm:

   ```text
     java/util/jar/JarFile.<init>(Ljava/io/File;)V
         lost at native-io/src/zip_real_jar.rs:1366              [bridge]
         WINS at native-builtins/src/phases_late/jar_manifest.rs:27  [bridge]
     java/util/jar/JarFile.<init>(Ljava/io/File;Z)V              ... same pair
     java/util/jar/JarFile.<init>(Ljava/lang/String;)V           ... same pair
     java/util/jar/JarFile.<init>(Ljava/lang/String;Z)V          ... same pair
   ```

   **`register_p59_jar` wins all four shared shapes; `register_jar_natives`'s
   four are dead in the shipping VM.** Price a `JarFile` retirement against
   `jar_manifest.rs`, and read `zip_real_jar.rs`'s constructors as what they
   are — unreachable. (The two-arity-only shapes `(File,Z,I)V` and
   `(File,Z,I,Runtime$Version)V` are registered by `register_p59_jar` alone
   and are not in the shadowed census.) `register_jar_natives` also mirrors
   its surface onto `java/util/zip/ZipFile`; both classes are under
   `java/util/`, so the registrar is L1's and crosses no lane boundary.

   The count-based ratchets in that file tolerate 1,201 shadowed rows, so this
   pair was invisible individually — the same shape as the `ZoneInfoFile`
   duplicate whose note in `native-builtins/src/lib.rs` ends "the later call
   always wins silently".

   **`BreakIterator` (17) was item 6's blocker wearing a different mask, and
   wave 5 implemented the fix.** See the item 6 entry below. One row of `apps/probes/L1JarTextSweep.java`
   is already waiting for whoever does — `E.attributesFromJar` reads `null`
   where HotSpot reads the jar's per-entry manifest section, and it is
   `JarFile`'s, not `JarEntry`'s: the class declares no native for
   `getAttributes`, so nothing wave 4 could retire repairs it.

   **Neither `+34` nor `+16` is thirty-four or sixteen wrong answers.** Both
   are ONE throw and a truncated section, which is why the prefix numbers
   looked so much worse than the families are:

   ```text
     java/util/jar/JarFile armed
       SECTION-DIED.zipAndJar  java.lang.NullPointerException
       — right after `jar.exists |true|`, i.e. on `new JarFile(f)` itself
     java/text/BreakIterator armed
       SECTION-DIED.text       java.lang.AbstractMethodError
   ```

   The `AbstractMethodError` is the tell, and it links this item to item 6:
   `BreakIterator` is ABSTRACT, and yielding `getWordInstance` sends real
   bytecode to the locale provider for a concrete
   `sun.text.RuleBasedBreakIterator` it does not get. That is the SAME
   provider lookup item 6's five vacuous rows are about — one blocker, two
   families — and it is the second time this lane has watched a fabricated
   abstract receiver trade a missing object for an `AbstractMethodError`.
   Fix the provider lookup and both move.
6. **`Date`/`TimeZone`/`sun/util/calendar/` (40)** and **`Locale` + providers
   (35)**: read the locale-provider trap below before pricing either — and
   start from the bisection, which is now taken. Armed one class at a time on
   `LocaleDateTzShadowSweep` (base 2 diffs), against
   `cratonvm-l1hm-base-20260911`:

   ```text
     java/util/Locale                       +117 WORSE  rc 1   reached=58
     sun/util/calendar/ZoneInfo               +4 WORSE         reached=5723
     java/util/TimeZone                       +2 WORSE         reached=53
     java/util/Currency                       +2 WORSE         reached=4
     sun/util/calendar/ZoneInfoFile           +0 same          reached=3792
     java/util/Date                           +0 same          reached=0  VACUOUS
     sun/util/locale/provider/CalendarDataUtility      +0      reached=0  VACUOUS
     sun/util/locale/provider/JRELocaleProviderAdapter +0      reached=0  VACUOUS
     sun/util/locale/provider/LocaleResources          +0      reached=0  VACUOUS
     sun/util/resources/Bundles                        +0      reached=0  VACUOUS
     sun/util/resources/LocaleData                     +0      reached=0  VACUOUS
   ```

   `java/util/Locale` is the family's whole weight and it takes the probe
   down with it. **`sun/util/calendar/ZoneInfoFile` is the one real
   candidate** — `+0` with 3,792 door engagements, which is a green that
   means something. Every one of the five PROVIDER classes read `+0` with
   `reached == 0`: the probe never asks them anything, so those five rows are
   §7's trap and say nothing at all. That is the measurement the trap
   predicted, and the next step on this family is a probe that reaches a
   provider lookup, not another sweep.
   **WAVE 5 LOCATED THE PROVIDER BLOCKER AND IMPLEMENTED HALF OF IT.** The
   five `reached == 0` rows above are one thing, and it is not a provider
   *lookup* at all — it is a bundle *read*:
   `LocaleResources.getBreakIteratorInfo` and its sibling
   `getDateTimePattern` answer `null` because the class-based bundle they
   need is looked for in a package the image does not have.

   Three places in the tree blame "jdk.localedata's class-based resource
   bundles are not surfaced through our jimage path"
   (`vm/src/vm/vm_exec.rs` BREAKITER and BUG-15,
   `native-builtins/src/reflect_annotations.rs`). **That claim is stale**, and
   W7-80 already proved it stale for the CLDR families in
   `locale_resources.rs` — it just never propagated to the other three files.
   The classes load, instantiate and evaluate out of the jimage this VM boots
   from. What actually missed is narrower and fixable:
   `cldr_packages` maps EVERY `sun.text.resources.*` base name onto
   `sun/text/resources/cldr{,/ext}`, deliberately and correctly for the
   families CLDR re-generated — and `BreakIteratorInfo` is not one of them.
   Verified with `jimage list` on a JDK 25.0.3 image rather than assumed:

   ```text
     java.base       sun/text/resources/BreakIteratorInfo.class
                     sun/text/resources/{Word,Line,Sentence}BreakIteratorData
     jdk.localedata  sun/text/resources/ext/BreakIteratorInfo_th.class
                     sun/text/resources/ext/{Word,Line}BreakIteratorData_th
   ```

   and no `cldr` sibling of either. So every candidate missed and the family
   read as absent. `CollationData` is the same shape, which is why
   `cldr_collation_rule` had to hand-roll its own probe instead of calling
   `load_cldr_table` — one symptom already in the tree, unexplained.

   Wave 5 adds `non_cldr_packages`, consulted first by `load_cldr_table`, and
   two natives that answer the exact pair
   `BreakIteratorProviderImpl.getBreakInstance` reads — both force-listed in
   the two dispatch gates, because **a registration on a concrete JDK-library
   method is silent without a gate entry**. The chain, read off the image's
   own bytecode with `javap -c` rather than from memory (the descriptor is
   `(Ljava/lang/String;)Ljava/lang/Object;`, not the `[Ljava/lang/String;`
   this lane would have guessed):

   ```text
     getBreakInstance(locale, idx, dataKey, dictKey)
       LocaleResources.getBreakIteratorInfo("BreakIteratorClasses") -> String[3]
       LocaleResources.getBreakIteratorInfo(dataKey)                -> String
       LocaleResources.getBreakIteratorResources(dataKey)           -> byte[]
       new sun.text.RuleBasedBreakIterator(name, bytes)
   ```

   `getCharacterInstance` is NOT in it — JDK 25 answers that one with an
   inline `GraphemeBreakIterator` and asks no bundle — which is why the root
   bundle's `BreakIteratorClasses` has exactly three entries and the three
   remaining factories pass indices 0/1/2.

   **What is implemented and what is not.** The two readers are. The last
   step is real `sun.text.RuleBasedBreakIterator` bytecode over the byte[],
   and that is unmeasured: it needs a trial binary, and until it runs, the
   BREAKITER allow-list stays where it is. Retiring `java/text/BreakIterator`'s
   17 is the step AFTER that measurement, not part of it.

   The same `non_cldr_packages` hook is what `getDateTimePattern` would need
   to stop answering from its hardcoded en/de table, which is item 8's route
   too. Neither is done here.

7. **`HashSet`/`LinkedHashSet` (13 + lane T's 42)**: blocked on lane T
   releasing `register_hashset_natives`. Nothing to RETIRE until then.

   **The one measured row is repaired in wave 5, and the diagnosis this item
   carried was wrong.** `L1MapFieldProbe`'s `F.hashSet.backing.fields` reads
   `threshold = 16` where HotSpot reads 12 for
   `new HashSet<>(List.of("a","b","c"))`. This item said "this VM writes the
   capacity into the threshold". It does — and so does HotSpot: while
   `table == null` the JDK deliberately overloads `threshold` to mean "the
   size the first `resize()` will allocate", and `map_state`'s capacity
   reader depends on exactly that overload. The constructor is right.

   What is missing is the OTHER half of `HashMap.resize()`. `map_resize`
   ends by publishing the new table and the new size, and writes `threshold`
   only on the `Hashtable` branch — so on the HashMap family the field keeps
   whatever the constructor parked there and goes stale the moment the table
   changes size. It is not a HashSet defect at all: **every grown HashMap in
   this VM has a stale `threshold`**, and the HashSet row is simply where a
   probe happened to read one. Wave 5 adds the write, with `resize_threshold`
   extracted as a pure function so the arithmetic has a test that needs no
   heap (including the non-0.75 load factors and the NaN the constructor
   screen would have refused).

   Retiring the rows still waits on lane T. Fixing a wrong answer did not.
8. **`ResourceBundle` (12)**: §8. Blocked on the same locale-provider
   lookup as item 6's `Locale`, and it is the cheapest probe into it —
   `TimeZone.getDisplayName` is one call and the answer is one string.

### And one that is not a family at all

`try_delegate_real_collection` in `native-collections/src/lib.rs` is the
helper every collection `size`/`isEmpty` native calls when it finds no
synthetic backing, and its opening premise —

> `invoke_special` does an *exact* per-class native lookup (which finds
> nothing for these real classes)

— is FALSE for every `SET_VIEW_CARRIERS` entry, because this VM registers
natives under the real JDK class NAME. So on a real `HashMap$EntrySet` the
helper re-finds the very native that called it, trips its own re-entrancy
guard, and returns the sentinel it exists to avoid: `size()` answers 0 for a
three-entry map, four frames below a `size()` that answers 3.

`invoke_special_bytecode_only` is the API for precisely this case — its own
doc says "for a native that IS ITSELF the native registered for
(class, method, descriptor)" — and the fix is one line. Wave 3 did not take
it, because wave 3 measured 0 diffs without it and this helper sits under
every collection in the VM; it wants its own control, its own trial and its
own corpus run. §3's wave-3 trace is the evidence, and a comment in the
source is not a compile-time link to the premise it depends on.

**TAKEN IN WAVE 5, in the narrowest form that fixes it.** Not an
unconditional swap: the bytecode-only route is taken only when the
RECEIVER'S OWN CLASS declares the method
(`ctx.class_declares_method(ctx.class_id_of_object(this), …)`). Every
`SET_VIEW_CARRIERS` entry does, and that is the input the premise is false
for. A receiver that only INHERITS the method resolves up to a supertype
that may be abstract or bodiless, and for those `invoke_special`'s
native-first lookup is still the safer answer and is unchanged — that is
the input the sentence was written about.

The premise is now a test rather than a comment:
`every_set_view_carrier_has_the_natives_that_falsified_the_delegate_premise`
asks the registry whether each carrier really does have `size`/`isEmpty`
registered under its real JDK name. If a future change stops registering
them, the guard goes red and says the branch can be simplified back.

**Unmeasured.** It wants exactly the control, trial and corpus run this
paragraph always said it did.

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

The CLDR provider failures in the corpus are in this lane's territory, and the
L7 gate on them is **lifted as of 2026-09-10**: `jdk/internal/loader/BuiltinClassLoader`
links, and `Class.getName` is a reviewed `Intrinsic` (it was recorded as tagged
a day earlier and was not — see the
[`the-builtin-classloader-could-not-link-and-getname-was-never-tagged-20260910`](../jdk-only/the-builtin-classloader-could-not-link-and-getname-was-never-tagged-20260910.md) record §2).

**Re-price them; do not read the pre-2026-09-10 failure text.** One CLDR symptom
in particular is now yours to own rather than to wait on:
`System.out.printf` -> `java.util.Formatter` -> `DecimalFormatSymbols.getInstance`
-> `LocaleProviderAdapter.forType` throws
`ServiceConfigurationError: Locale provider adapter "CLDR" cannot be instantiated`
under `CRATONVM_ENFORCE_NATIVE_SHADOW=all`. That is not a loader failure; it
takes out every armed vector and every armed PROBE that formats a string, so it
is worth pricing before the retirement rows.

## 11. Standing warnings this lane confirmed

- **A Rust side table is not the object.** §9's items 1 and 2 are the same
  defect `java/util/Properties` had, and the remedy is the same: make the real
  object the authority, THEN drop the native.
- **A "we don't surface X" comment outlives the day someone surfaced X, in
  every file the fix did not touch.** W7-80 measured that the image's own
  CLDR bundle classes load fine and rewrote the comments *in
  `locale_resources.rs`*. Three other files still said the opposite a month
  later — `vm_exec.rs` twice and `reflect_annotations.rs` once — and one of
  them is a load-bearing allow-list that pins natives over real bytecode on
  the strength of the stale claim. When a measurement falsifies a premise,
  grep the premise's WORDS across the tree, not the function that held it.
- **`javap` the image before writing a descriptor, even a boring one.**
  `LocaleResources.getBreakIteratorInfo` returns `Ljava/lang/Object;` and the
  caller `checkcast`s it; the obvious guess is `[Ljava/lang/String;`, and a
  registration on the wrong descriptor is not an error, it is silence.
- **A blanket mapping that is right for the re-generated families is wrong
  for the ones that were not.** `cldr_packages` sends every
  `sun.text.resources.*` name to a `cldr` package on purpose and correctly.
  `BreakIteratorInfo` and `CollationData` have no `cldr` sibling, so for them
  every candidate missed and the family read as ABSENT rather than as
  misrouted. The tell was already in the tree: `cldr_collation_rule` had had
  to hand-roll its own probe, and nobody asked why.
- **A stored derived quantity forces you to shadow every mutator.** Retire a
  class's mutators and its derived reads in one wave, or the two disagree.
- **Views and iterators are one unit with their backing class.** Every wave
  here moved `$SubList`, `$ListItr`, `$Itr` with `ArrayList`, and
  `$EmptyIterator`/`$EmptyListIterator`/`$EmptyEnumeration` with `Collections`.
- `toArray` is registered on 34 classes and **the dispatch route decides the
  exception message**, so a probe must print the message, not the outcome.
- `java/util/Hashtable` is still **held** on purpose, and the held-family test
  says so. Amend that test rather than deleting the entry.
- **An armed dial arm that goes RED is a candidate, not a verdict — the same
  way a green one is not.** The ops page's rule was written for vacuous
  greens; wave 3 is the mirror image. `java/util/HashMap` was held for two
  revisions of this page on nine rows the dial moved and the trial binary does
  not. The dial declines at nine DISPATCH DOORS; retirement removes the
  REGISTRATION. Those differ wherever the deciding call starts inside a
  native, and a family whose natives call each other is exactly where they
  differ most.
- **When a native asks its receiver a question, that question has no door.**
  It is §8's rule stated from the other end, and it is the reason a fallback
  path can answer 0 while the same call from Java answers 3. If an armed arm
  is red and the rows all funnel through one VM-internal question, ask what
  answers that question before believing the red.
- **A typed destination is a free route discriminator.**
  `coll.toArray(new String[0])` on a collection of non-Strings throws
  `ArrayStoreException` from real bytecode and cannot throw from a native that
  believes the collection is empty. One row separated "the view is empty" from
  "the walk is empty" after two revisions of not knowing.

## 12. Done

For this lane: every bucket-A/B row in the prefix set is retired, or classified
with the measurement that refused it and the blocker named. That is §1, and it
is complete. What remains is the eight VM changes in §10 — each one a piece of
engineering with a stated acceptance test, not an open question.

Waves 3 and 4 moved three families out of "held, reason unknown" and into
"held, blocker named and located", which is the only kind of hold worth
keeping:

```text
  HashMap's 77    the iterator carriers are shared with HashSet and Hashtable
                  (key_itr_carrier_for), and LinkedHashMap inherits eight
                  methods whose entries live in lhm_overlay()
  JarFile         new JarFile(f) itself NPEs under the yield
  BreakIterator   AbstractMethodError — the locale provider hands back no
                  concrete RuleBasedBreakIterator, the SAME lookup that makes
                  item 6's five provider rows vacuous
```

None of those is "9 probes move". Each names a function and a file.
