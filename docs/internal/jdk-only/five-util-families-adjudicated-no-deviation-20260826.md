# Five more retirement-surface families adjudicated: no observable deviation, 92 of 192 rows exercised

**Status: MEASURED 2026-08-26.** No source change. Companion to
`the-retirement-surface-is-2446-and-has-code-is-not-grounds-20260826.md`, which
sized the surface and set the method.

## 1. What was probed, and why these five

From the 2,057 **`bridge`**-kind rows in the `--jdk-only` retirement surface —
the kind the shadow census actually counts — by class size:

```text
 87  jdk/internal/misc/Unsafe          41  java/util/TreeMap
 82  sun/misc/Unsafe                   40  java/nio/file/Files
 63  java/lang/Class                   37  java/util/Properties
 61  java/lang/AbstractStringBuilder   33  java/util/ArrayDeque
 58  java/lang/StringBuffer            31  java/util/LinkedList
 57  java/lang/StringBuilder
 48  java/util/concurrent/ConcurrentHashMap
```

`probes/UtilFamilySweep.java` takes **Properties, BigInteger, ArrayDeque,
LinkedList and TreeMap** — 102 value assertions in one deterministic run,
diffable on stdout, needing no rebuild.

**The StringBuilder/StringBuffer/AbstractStringBuilder cluster (176 rows) is
deliberately excluded.** `WORKER-3-NOTE-3` already had it OPEN (**closed 2026-08-28 by lane L2**) with a diagnosed
mechanism — armed across the three classes, every `append` was discarded and
`toString()` returned empty with `rc=0`. Probing a family that is already under
investigation would have re-derived a known answer. Reading the history first is
the rule from the companion record, and this is the second time in a row it
saved a cycle.

## 2. Result

```text
HotSpot 25.0.3+9      102 lines
CratonVM compatible   0 differing lines
CratonVM --jdk-only   0 differing lines
```

And the check that makes a zero-diff mean anything — invocations taken **in the
same run as the probe**, not from an earlier one:

| class | owning rows | rows that RAN | invocations |
| --- | ---: | ---: | ---: |
| `java/util/Properties` | 37 | 14 | 25 |
| `java/math/BigInteger` | 39 | 26 | 127 |
| `java/util/ArrayDeque` | 34 | 15 | 22 |
| `java/util/LinkedList` | 37 | 14 | 23 |
| `java/util/TreeMap` | 45 | 23 | 36 |
| **total** | **192** | **92** | **233** |

So 92 rows served 233 calls and every value matched HotSpot, including the
cases most likely to diverge: `Properties`' three-way split between `get`,
`getProperty` and `stringPropertyNames` over a defaults chain and a non-String
value; `BigInteger.modPow` / `modInverse` / `doubleValue` at raw bits;
`ArrayDeque.descendingIterator`; `LinkedList.hashCode` agreeing with
`ArrayList`'s; `TreeMap`'s navigation and view families; and eight
exception-shape checks.

## 3. What this does and does not license

**Does:** these 92 rows have no observable deviation and need no fix. After
eight real defects in the four families probed before them
(`Handler.getLevel`/`setLevel`, `PrintStream.charset`, `HttpURLConnection`
`setDoInput`/`getRequestProperty`, and four in `java.io.File`), a clean sweep is
worth recording — it bounds where the remaining defects are not.

**Does not:** license retiring any of them.

* 100 of the 192 rows were never reached. The count is a floor on correctness,
  not a coverage claim.
* Behavioural agreement is not the retirement test. `StrictMath` was
  bit-identical over 1,709 comparisons and is a KEEP, because its natives exist
  for a measured reason its registrar records.
* Three of these five are in the **collections ownership cluster**, where
  retirement by `NativeKind` is already DISPROVED (`HANDOFF-20260820` §0):
  `SyntheticStub` removes a *registration*, not a Rust function, and 168 direct
  Rust calls from 18 files plus 45 `try_alloc_concurrent_synthetic` sites bypass
  the registry entirely. After a retag those objects meet real bytecode over a
  real, empty table and answer **absent** — a silently empty map, no error, no
  failing vector.
* `Properties` is entangled with `vm_init`'s "LAST-WRITE-WINS BOUNDARY — do not
  reorder" block, which re-registers `properties_sidetable` *because*
  `register_collections_natives` overwrites it with layout-wrong versions.

`java/math/BigInteger` is the only one of the five with none of those
entanglements, and is the reasonable next candidate for a registrar-history
read.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out UtilFamilySweep
```
