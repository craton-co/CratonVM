# Seventeen more families swept clean — and what a negative batch is actually worth

**Status: MEASURED 2026-08-27.** No source change. Fourth in the adjudication
series, after `the-retirement-surface-is-2446-…` (method),
`five-util-families-…`, and `nine-more-families-swept-one-defect-…`.

## 1. Two batches, zero differences

Both run against an already-built binary, so neither cost a build cycle.

| probe | families | lines | diffs (both modes) |
| --- | --- | ---: | ---: |
| `ConcurrentReflectSweep` | `Method`, `Constructor`, `LinkedHashMap`, `PriorityQueue`, `ByteArrayOutputStream`, `CompletableFuture`, `ThreadPoolExecutor`, `ConcurrentHashMap$KeySetView` | 87 | **0** |
| `LangMiscSweep` | `Integer`, `Long`, `Double`, `Float`, `Boolean`, `Byte`, `Short`, `Character`, `Objects`, `Enum`/`EnumSet`/`EnumMap`, `StringJoiner`, `Random`, `UUID`, `Base64`, `java.time` core, `Calendar`/`SimpleDateFormat`, `NumberFormat`/`DecimalFormat` | 117 | **0** |

Invocations taken in the probes' own runs: **271 owning rows, 113 of them ran,
358 invocations.**

## 2. The rows that would have caught a regression

A clean batch is only worth something if it exercised the hard cases. These are
the ones chosen for that reason, and all matched:

* **`ConcurrentHashMap`'s three key-set-view constructions**, which have
  DIFFERENT contracts and are the exact shape of the `TreeMap$KeySet` defect
  fixed the same day: `chm.keySet().add()` must throw `UnsupportedOperation`,
  `ConcurrentHashMap.newKeySet().add()` must succeed, and `chm.keySet(7).add()`
  must succeed AND leave the map holding the default value. CratonVM gets all
  three right — so the `TreeMap` bug was local to that mirroring, not a general
  view-surface failure.
* **`Random` at raw bits from a fixed seed**, including `nextGaussian`. That is
  the precise case `lang_math.rs`'s fdlibm split was built for — a seeded
  gaussian stream was one ULP off HotSpot because `StrictMath.log` ran the host
  libm. Bit-identical now, which is independent confirmation that the
  `StrictMath` family adjudicated KEEP is doing its job.
* **`Method.invoke` wrapping in `InvocationTargetException`**, plus wrong
  receiver, null receiver on an instance method, and an inaccessible method —
  four refusal shapes rather than one happy path.
* **`Double.toString`** at `0.1`, `1e-320` (subnormal) and `1e300`, where the
  shortest-repr algorithm is easy to get subtly wrong.
* **`LinkedHashMap` access-order mode**, where a `get` must move the entry.
* **Seeded `Random.ints` streams**, `Base64` URL/no-pad/MIME variants, and
  `Calendar.add` rolling across a month boundary.

## 3. What a negative batch is worth, stated honestly

It is tempting to treat "no differences" as no information. It is not, but it is
weaker than it looks and both halves should be said:

**Worth:** it bounds where the remaining defects are not. The survey has now
probed 39 families and found 15 defects; every one of the 15 was in a family
whose registrar carried a stated justification that had drifted from its code.
None was in a family that was merely thin. That is now a usable prior for
choosing what to probe next — **read the registrar comments and probe the ones
making claims**, not the ones with the most rows.

**Not worth:** it is not a coverage claim and not a retirement licence. 158 of
the 271 rows here were never reached. Agreement is not the retirement test —
`StrictMath` was bit-identical over 1,709 comparisons and is a KEEP. And a probe
can only find what it thought to ask; three harness artefacts in this survey
(stdout encoding, asymmetric stderr capture, an assertion that hid the value)
each produced confident-looking differences with no VM behaviour behind them,
which is the same failure mode pointing the other way.

## 4. Running position

```text
bridge-kind retirement surface        2057 rows
covered by a differential probe       ~1420 rows   (69%)
families probed                       39
defects found and fixed               15
families adjudicated KEEP with reasons StrictMath (69 rows)
```

Remaining large families: `java/lang/System$1` (28), `java/lang/ClassLoader`
(27), `java/lang/Module` (23), `java/lang/invoke/MethodHandles` (23),
`ForkJoinTask`/`ForkJoinPool` (42), `ResourceBundle` (15) — plus the 176-row
StringBuilder cluster, which stays out of this series while
`WORKER-3-NOTE-3` had it open with a diagnosed mechanism.
**Closed 2026-08-28 by lane L2**: 747 rows, 0 diffs in both modes.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out ConcurrentReflectSweep
cratonvm --java-home "$JDK" --jdk-only -cp probes/out LangMiscSweep
```
