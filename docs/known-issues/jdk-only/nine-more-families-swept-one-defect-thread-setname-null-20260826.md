# Nine more retirement-surface families swept: 644 assertions, one defect — `Thread.setName(null)`

**Status: the defect is FIXED 2026-08-26.** The other eight families showed no
deviation. Third in the series after
`the-retirement-surface-is-2446-…` (method) and
`five-util-families-adjudicated-no-deviation-…`.

## 1. Batch method

Both probe batches run against a binary that was **already built**, so a family
costs a probe and two runs rather than a 30-minute rebuild. Fixes are then
batched into ONE build-and-verify cycle. That is the difference between
adjudicating one family per cycle and nine.

| probe | families | assertions | diffs |
| --- | --- | ---: | ---: |
| `ReflectBufferSweep` | `Class`, `reflect.Field`, `ByteBuffer`, `Thread` | 316 | **1** |
| `StringMapSweep` | `String`, `ConcurrentHashMap`, `Arrays`, `Collections` | 328 | 0 |

Identical in compatible and `--jdk-only` in both batches, so nothing here is a
mode defect.

Invocations taken **in the same run as the probe**, which is what makes a
zero-diff mean anything:

| class | rows | ran | invocations |
| --- | ---: | ---: | ---: |
| `java/lang/Class` | 93 | 30 | 438 |
| `java/lang/reflect/Field` | 36 | 10 | 72 |
| `java/nio/ByteBuffer` | 76 | 8 | 35 |
| `java/lang/Thread` | 58 | 16 | 30 |
| `java/lang/String` | 24 | 15 | 597 |
| `java/util/concurrent/ConcurrentHashMap` | 48 | 25 | 236 |
| `java/util/Arrays` | 19 | 12 | 111 |
| `java/util/Collections` | 25 | 10 | 13 |
| **total** | **379** | **126** | **1532** |

## 2. The defect

```text
new Thread(…).setName(null)
  HotSpot 25.0.3+9   NullPointerException
  CratonVM           accepted, silently, in BOTH modes
```

**Two defects in one line, not one.** `Thread.setName`'s first statement is
`if (name == null) throw new NullPointerException("name cannot be null")`. The
native had no check, so:

* the throw never happened; and
* the null was then written into the `name` field, so `getName()` answered
  **null** afterwards — a `Thread` whose name is null is a state the JDK's own
  API cannot produce, and every caller that formats a thread name is entitled to
  assume it cannot.

Fixed by rejecting a null argument before either write.

## 3. Eight clean families, and why that is worth writing down

Zero deviations across `Class`, `Field`, `ByteBuffer`, `String`,
`ConcurrentHashMap`, `Arrays` and `Collections` — including the rows most likely
to drift: `Class.getCanonicalName`/`descriptorString`/`arrayType` over eleven
class shapes (primitives, `void`, nested arrays, an enum, a member interface);
`Field.setInt` on a `String` field and `Method.invoke` with a wrong arg type,
both as exception shapes; `ByteBuffer` endianness, `slice`/`duplicate`/
`asReadOnlyBuffer`/`compact`, and four underflow/read-only refusals;
`String.split` with and without a negative limit; `CHM`'s
`compute`/`merge`/`replace(k,old,new)` and its three null refusals.

After nine defects found in five families earlier in this sweep, the useful
content of a clean batch is that it **bounds where the remaining defects are
not**. The running tally over the whole survey: 13 families probed, 10 defects,
and every one of the 10 was in a family whose registrar carried a stated
justification that had drifted from its code — none was in a family that was
simply thin.

## 4. Still not a retirement licence

Same three reasons as the previous record, unchanged: 253 of the 379 rows here
were never reached; agreement is not the retirement test (`StrictMath` is
bit-identical and a KEEP); and the registrar's own history has to be read first
— which is what kept the 176-row StringBuilder cluster out of both batches,
since `WORKER-3-NOTE-3` already has it open with a diagnosed mechanism.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out ReflectBufferSweep
cratonvm --java-home "$JDK" --jdk-only -cp probes/out StringMapSweep
```
