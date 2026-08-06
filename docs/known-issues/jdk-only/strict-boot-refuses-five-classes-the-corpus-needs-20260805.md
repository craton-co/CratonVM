# `--jdk-only` now refuses five classes the corpus needs — 9 sections regressed

| | |
|---|---|
| **Status** | CLOSED 2026-08-05 — all five classes fixed, in two changes. `probes/JdkOnlyCollectionViewProbe` is byte-identical to HotSpot 25 in BOTH modes. See "What was fixed" and "The second half" below |
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

## The second half, 2026-08-05 — and why both deferrals were wrong

The two families above were filed as needing separate waves. Both reasons were
derived by reading the code. `probes/StrictIterPrimitivesProbe` measures the
same claims under `--jdk-only` instead, and neither survived.

**The iterator family had nothing to do with `table[]`.** The deferral said a
real iterator would read a `table[]` that `HashMap.put` never fills and return
a silently EMPTY iteration. What the probe found is that the refusal was not a
policy at all — it was *order-dependent*:

```
asList.iterator.class=java.util.HashMap$KeyItr     <- host JDK: java.util.Arrays$ArrayItr
linkedhashset.iterator=[s1|s2]/2                   <- passes here, NoClassDefFoundError elsewhere
```

`make_iterator_from_array` minted `java/util/HashMap$KeyItr` through the
INFALLIBLE `alloc_synthetic`, while three sibling sites used the fallible one.
So `Arrays.asList(a).iterator()` created the class, and every later
`try_alloc_synthetic` for that name then *found* it and succeeded. That is why
`linkedhashset.iterator()` passed in a probe that had called `Arrays.asList`
first and threw in one that had not — and why the section list above looks
arbitrary.

The fix routes all four sites through the refusal and gives the refusal
somewhere to land. The snapshot is already an `Object[]`, and the JDK ships the
fixed-size list for exactly that shape: wrap it in a real `Arrays$ArrayList` and
ask THAT for its iterator — with `invoke_virtual_bytecode_only`, so the
`Arrays$ArrayList.iterator` override does not hand back the fabricated class
again. The result is a real `java.util.Arrays$ArrayItr`.

**`StreamCollector` did not need a `Consumer` substitute — it needed no
`Consumer`.** The deferral was right that nothing can stand in for it (it is
VM-internal and a side table cannot be a `Consumer`) and right that
`StreamSupport.stream(spl, false).toArray()` would discard the safety cap. It
missed the third option: `java.util.Spliterators.iterator(Spliterator)` is
public API, carries no native override, and returns a real
`Spliterators$1Adapter` that is *itself* both the iterator and the consumer.
The probe measures that adapter working under `--jdk-only`
(`adapter.class=java.util.Spliterators$1Adapter`), which is what makes it
viable where a hand-built consumer is not. Driving `hasNext`/`next` from the
native keeps the 1,000,000-element cap exactly as it was, so the objection does
not apply. The accumulator is a real `ArrayList`, not a Rust `Vec`, because
every `next()` re-enters Java and may move the heap.

All three `StreamCollector` mint sites now ask the policy first, including the
two that went through the infallible funnel — leaving any one of them unguarded
is precisely what made the iterator refusal order-dependent.

Measured, Windows / Temurin 25.0.3, diffed against the host JDK:

| probe | mode | before | after |
|---|---|---:|---:|
| `JdkOnlyCollectionViewProbe` | `--jdk-only` | 7 diverging lines | **0** |
| `JdkOnlyCollectionViewProbe` | `--real-jdk` | 0 | 0 |
| `StrictIterPrimitivesProbe` | `--jdk-only` | 4 | **0** |

`--real-jdk` is untouched by design: both fallbacks fire only on a policy
refusal, which only happens under `--jdk-only`.

One behaviour genuinely changes, and loudly rather than silently: `remove()` on
a strict-mode snapshot iterator now raises `UnsupportedOperationException` from
real JDK bytecode instead of writing through to the backing collection. On the
strict path the alternative was never a working `remove()` — it was an
iteration that did not reach `next()`.

### The `--real-jdk` residual — CLOSED 2026-08-06, and it was a 5.4x speedup

`StrictIterPrimitivesProbe` recorded one divergence that survived the change
above: `Arrays.asList(a).iterator().getClass()` reported
`java.util.HashMap$KeyItr` where HotSpot says `java.util.Arrays$ArrayItr`,
because default mode still fabricated the iterator and only the refusal path
reached a real one. It was left open on the grounds that converting the idiom
would change every snapshot iterator in the VM and so deserved its own
measurement.

It did. The measurement went the other way: building the real class is not a
cost, it is **5.4x faster** (`probes/SnapshotIteratorCostProbe`, 50k iterations
× 8 elements, interleaved in both orders across four alternations, identical
checksums in every arm):

| arm | ms |
|---|---:|
| fabricated `HashMap$KeyItr` | ~655 |
| real `Arrays$ArrayItr` | **~120** |
| HotSpot 25, for scale | ~1–11 |

At 300k iterations the fabricated arm also degrades across rounds (3537 → 4046
→ 4302 ms) where the real one stays flat.

The reason is not the field write the arithmetic suggested. **A fabricated class
has no bytecode**, so `hasNext`/`next` have to be registered natives — two
`safe_native_call` trips per ELEMENT. The real class runs ordinary bytecode the
JIT compiles and inlines. A fabricated stand-in is a performance tax and not
only a fidelity bug, and that generalises past this site: wherever a
`cratonvm/internal/*` or invented `java.util.*` class carries natives that a
real class would execute as bytecode, the honest version is likely the fast one
too.

Choosing the replacement is the whole trick, and the rule is narrower than "use
a real class": it has to be one whose fields are declared and writable BY NAME,
so filling them is construction rather than fabrication —

```text
java.util.Arrays$ArrayItr                             cursor:int  a:Object[]
java.util.concurrent.CopyOnWriteArrayList$COWIterator cursor:int  snapshot:Object[]
```

`probes/SnapshotIteratorShapeProbe` is the oracle, and it caught a second
divergence it EXPOSED rather than introduced: `CopyOnWriteArrayList.iterator()`
reported the generic iterator where HotSpot says `COWIterator`. It was equally
wrong before, as `HashMap$KeyItr`; no probe had covered it. Diffing only the one
line named in this record would have shipped a differently-wrong answer and
called the residual closed.

## Verification, on both platforms

The strict gate on Azure Linux went from 9 failed sections to a PASS with
**17 sections reported as no longer diverging**, and the baseline was re-frozen
as the gate demands. The bridge ratchet is unchanged at its baseline (9697 /
4688) — no registration was added or removed.

Regression corpus, Windows / Temurin 25.0.3, with each change measured
separately:

| corpus | before | + iterator & collector | + `PrintStream.append` |
|---|---:|---:|---:|
| default | 28 / 0 | 28 / 0 | 28 / 0 |
| `--jdk-only` | 24 passed, 25 failed | 31 / 18 | **32 / 17** |

Every failure list is a strict subset of the one before it — nothing newly
fails. The eight recovered classes are `RExecutorShutdown`,
`RChannelInterrupt`, `RMapResizeGc`, `RMapGcStress`, `ROverlaySystemGcStress`,
`RFileTimes`, `RJdkRecords` (the iterator families) and `RJdkHello` (the
`PrintStream.append` defect filed alongside this one). Azure Linux lands on the
same 32 / 17 with the same list.

`RSerial` still fails under `--jdk-only` on `cratonvm/internal/SystemLogger`.
It failed identically before this change, so it is not a regression from it;
it is the `serialization` row, which no snapshot iterator is involved in.

### One measurement trap this ran into

A first pass reported the corpus as 0 passed for EVERY class, in both modes,
for every binary — and that was the harness, not the VM. Passing
`JDK=/c/Program Files/...` puts an MSYS-style path in front of a native Windows
binary, and because `regression-suite/run.sh` invokes it through `timeout`,
MSYS does not rewrite the argument. `--java-home` never resolved and every
class exited 1 identically. Use `JDK=C:/Program Files/...` on Windows. A
uniform zero across unrelated tests is a harness signature, not a VM one.

## Reproducing

```sh
JAVA_HOME=/path/to/jdk25 CV=target/release/cratonvm \
    bash scripts/jdk-only-strict-probes.sh
```

Before the fix: exit 5, and `target/jdk-only-strict-probes/logs/*.strict.diff`
carries the nine sections. A single class is enough to see it:

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
