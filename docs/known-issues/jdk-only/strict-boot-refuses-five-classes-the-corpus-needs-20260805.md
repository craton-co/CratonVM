# `--jdk-only` now refuses five classes the corpus needs — 9 sections regressed

| | |
|---|---|
| **Status** | OPEN, NARROWED 2026-08-05 — three of the five classes are fixed and the gate is down from 9 failed sections to 6. Two families remain, both named below |
| **Severity** | high — the first genuine **strict-only** regression this corpus has produced, and it is nine probe sections wide |
| **Modes** | `--jdk-only` ONLY. `--real-jdk` is byte-identical to HotSpot 25 on both probes |
| **Found** | 2026-08-05 by `scripts/jdk-only-strict-probes.sh`, on its first run against a merged `dev` |
| **Suspect** | L7 (`feat/l7-synthetic-class-migration-20260805`) — fabrication became refusable and a strict boot now fabricates zero compatibility classes |

## What happens

Two probes that were byte-identical to HotSpot in both modes an hour earlier
now fail nine sections under `--jdk-only`, every one with a
`NoClassDefFoundError`:

```
SECTION-FAILED collections:    java/util/HashMap$KeyItr
SECTION-FAILED interfaces:     cratonvm/internal/StreamCollector
SECTION-FAILED net:            java/util/HashMap$KeyItr
SECTION-FAILED concurrent:     java/util/HashMap$KeyItr
SECTION-FAILED text:           cratonvm/internal/ArrayListSubList
SECTION-FAILED regex:          cratonvm/internal/ArrayListSubList
SECTION-FAILED time:           java/util/HashMap$KeyItr
SECTION-FAILED textformat:     cratonvm/internal/UnmodifiableList
SECTION-FAILED serialization:  cratonvm/internal/SystemLogger
```

plus `VirtualMachine.list()` in the `agent` section, which went from
`InternalError` to `NoClassDefFoundError`.

Five distinct classes:

| class | reached from |
|---|---|
| `java/util/HashMap$KeyItr` | iterating any `HashMap`/`HashSet` key view — collections, sockets, executors, `java.time` |
| `cratonvm/internal/StreamCollector` | `Collection.stream()` on a user `AbstractCollection` |
| `cratonvm/internal/ArrayListSubList` | `String.split`, `List.subList` |
| `cratonvm/internal/UnmodifiableList` | `NumberFormat`/`DateFormat` |
| `cratonvm/internal/SystemLogger` | serialization |

`java/util/HashMap$KeyItr` is the widest by far: it is on the path of any
`for (K k : map.keySet())`, so this is not a corner.

## Why this is a real regression and not a probe problem

* Same probes, same class files, same JDK image, same gate invocation.
* The **`--real-jdk` arm of both probes is byte-identical to HotSpot 25** in
  the same run. Only the strict arm fails.
* Every arm exits 0, so this is a wrong-answer regression, not a hang or a
  crash: the `SECTION-FAILED` lines and the `failed=5` / `failed=4` tallies are
  the probes' own reporting doing its job.
* Immediately before merging `origin/dev`, the same gate on the same branch had
  both probes byte-identical in both modes, with only `JdkOnlyPlatformProbe`
  diverging on its four already-filed defects.

## The shape it has

L7's own hand-off note anticipated exactly this failure mode for one family:

> **A fabrication cannot be made fallible before its native is retagged**, when
> the class stands in for a real JDK method the JDK's own bootstrap calls.
> Measured: refusing `cratonvm/internal/Unmodifiable*` reaches a zero census
> and produces `NullPointerException: zone` from `java.time`, which names
> nothing.

`cratonvm/internal/UnmodifiableList` is in the list above, so the note's own
example is now failing — but it fails *loudly*, with the class name, rather
than as the unattributable NPE the note describes. Whatever changed between
that measurement and the landing made the failure legible; it did not make it
go away.

The other four names were not in that note, and `java/util/HashMap$KeyItr` is
not a `cratonvm/internal/*` name at all — it is a real JDK nested class, so
"stop fabricating compatibility classes" should not be refusing it. That one
may be a different defect wearing the same symptom.

## What was fixed, 2026-08-05 — three of the five, and the cause was duplicate registrations

L7 R1 retagged four registrars `SyntheticStub` so `--jdk-only` drops them and
`java.base`'s bytecode runs, then made the allocators refuse. **The retag missed
the duplicates of the same triples in other registrars, and those win by
last-write**, so the refusal landed on registrations that were still live:

| triple | registrar that kept it `Bridge` | class it minted |
|---|---|---|
| `java/util/ArrayList.subList(II)` | `register_arraylist_natives` | `ArrayListSubList` |
| `java/util/Collections.unmodifiableList` | `register_collections_utility_natives` | `UnmodifiableList` |
| `java/util/{List,Set,Map}.copyOf` | `register_collections_extras_natives` | `Unmodifiable{List,Map}` |

`--dump-native-registry` named all five with invocation counts. Re-reading the
retagged registrars would not have: they say exactly what L7 intended, and the
registrations that dispatch are somewhere else. This is the duplicate-
registration failure mode this repository keeps producing, one layer up — the
usual form is a fix that stops working, and this form is a fix that never
started.

The five are `SyntheticStub` now. Measured with
`scripts/jdk-only-strict-probes.sh`, same JDK image, same class files:

| | before | after |
|---|---:|---:|
| `SECTION-FAILED` lines | 9 | **6** |
| `CENSUSLOAD` failed sections | 5 | **4** |
| `PROBE2` failed sections | 4 | **2** |
| regression corpus, `CRATONVM_ARGS=--jdk-only` | 21 passed / 28 failed | **24 / 25** |
| regression corpus, default | 28 / 0 | 28 / 0 |

Sections recovered: `text`, `regex`, `textformat`. The corpus classes recovered
are `RCollections`, `RStrings` and `RJdkCollections`, and the failure list is a
strict SUBSET of the previous one — nothing newly broken.

`probes/JdkOnlyCollectionViewProbe` is the blast radius rather than the two
paths this was found through: 20 lines diverging from HotSpot 25 under
`--jdk-only` before, 7 after, and 0 in `--real-jdk` both times. It reports the
CONTENT of every view (`[b|c]/2`), so a real fallback that came back silently
EMPTY — L7's stated reason for leaving `HashSet.iterator()` alone — cannot read
as a pass.

The bridge ratchet moved in the good direction and was re-frozen in the same
change, as it demands: 9705 → 9697 and 4696 → 4688.

## What remains, and why neither is the same fix

* **`java/util/HashMap$KeyItr` and `java/util/TreeSet$Itr` — 4 of the 6 sections.**
  L7 R1 deferred this family explicitly and gave the reason: the real iterator
  reads the real `table[]`, which CratonVM's `HashMap.put` native never fills,
  so retagging it returns a silently EMPTY iteration instead of a loud error.
  That is worse than the current failure, and the fix is the collections
  reclassification wave, not a retag.
* **`cratonvm/internal/StreamCollector` — `interfaces`, and
  `cratonvm/internal/SystemLogger` — `serialization`.** Neither is a stand-in
  for a JDK class, so no retag can remove them. `StreamCollector` is a
  VM-internal `Consumer` handed to a REAL `Spliterator.tryAdvance` by
  `drain_spliterator_to_array` — the JDK needs an object of a `Consumer` type
  and a side table cannot be one. Draining through
  `StreamSupport.stream(spl, false).toArray()` instead would need no fabricated
  class, but it also discards the safety cap that function exists to enforce
  against an infinite spliterator, so it is a stream-subsystem change with its
  own evidence, not a line edit.

## Reproducing

```sh
JAVA_HOME=/path/to/jdk25 CV=target/release/cratonvm \
    bash scripts/jdk-only-strict-probes.sh
```

Exit 5, and `target/jdk-only-strict-probes/logs/*.strict.diff` carries the nine
sections. A single class is enough to see it:

```sh
cratonvm --jdk-only --java-home $JDK25 -cp <probes> JdkOnlyCensusLoadProbe
```

## Why it was caught

Criterion 6's gate had not existed before today. This regression would
otherwise have sat on `dev` behind a green build: no unit test covers it, the
`--real-jdk` suites are unaffected, and the advisory `jdk-only` job's existing
steps (registry census, class-origin census, stub ratchet) all still pass —
because a class the loader **refuses** is not a class it **fabricates**, so a
zero-fabrication census and a nine-section outage are the same number.
