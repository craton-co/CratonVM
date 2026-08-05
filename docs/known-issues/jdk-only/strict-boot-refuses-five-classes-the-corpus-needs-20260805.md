# `--jdk-only` now refuses five classes the corpus needs — 9 sections regressed

| | |
|---|---|
| **Status** | OPEN — a live regression on `dev`, found the same day it landed |
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
