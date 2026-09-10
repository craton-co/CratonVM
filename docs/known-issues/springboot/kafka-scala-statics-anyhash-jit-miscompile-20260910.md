# `scala.runtime.Statics.anyHash(Long)` answers garbage once JIT-compiled

**Status: OPEN, 2026-09-10.** A JIT miscompile with a 20-line reproducer that
needs no Kafka, no Spring and no broker. It is the reason
`org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests`
is a CratonVM-only Spring Boot failure; the same defect is reachable from any
Scala code on this VM, because `Statics.anyHash` is what `##` and every Scala
hash-based collection call for a boxed value.

## The suite symptom, and why it is not a timeout

`testEndToEnd` and `testEndToEndWithRetryTopics` both fail on

```text
org.opentest4j.AssertionFailedError: Expecting value to be true but was false
    at …KafkaAutoConfigurationIntegrationTests.testEndToEnd(…:93)
```

which is `assertThat(listener.latch.await(30, TimeUnit.SECONDS)).isTrue()` — a
30-second budget on one record reaching a `@KafkaListener` from the embedded
KRaft broker. That reads like host load. It is not: **3 of 3 runs fail alone on
an idle host** (load 2.9-6.4), and stock HotSpot 25 on the same classpath is
**2 of 2 clean**.

The broker log says what actually happened, 354 times in one run:

```text
ERROR kafka.server.KafkaApis -- [KafkaApi-0] Unexpected error handling request
  RequestHeader(apiKey=LIST_OFFSETS, apiVersion=11, …)
  ListOffsetsRequestData(… partitions=[ListOffsetsPartition(partitionIndex=0, currentLeaderEpoch=0, timestamp=-2), …])
java.util.NoSuchElementException: key not found: -2
	at scala.collection.immutable.BitmapIndexedMapNode.apply(HashMap.scala:674)
	at scala.collection.immutable.HashMap.apply(HashMap.scala:132)
	at kafka.server.ReplicaManager$.isListOffsetsTimestampUnsupported(ReplicaManager.scala:149)
	at kafka.server.ReplicaManager.fetchOffset(ReplicaManager.scala:1469)
	at kafka.server.KafkaApis.handleListOffsetRequest(KafkaApis.scala:820)
```

`-2` is `ListOffsetsRequest.EARLIEST_TIMESTAMP`, which the consumer sends
because the test sets `auto-offset-reset=earliest`. The broker cannot look up a
key that is in its own map, so the consumer never gets an offset, never
consumes, and the latch expires. The consumer side only ever sees "The server
experienced an unexpected error when processing the request., retrying."

## Reproducer

Twenty lines, `scala-library` on the classpath and nothing else:

```java
import scala.runtime.Statics;
public class AnyHashOnly {
  public static void main(String[] a) {
    int iters = Integer.parseInt(a[0]);
    Long v = Long.valueOf(-2L);
    int bad = 0, first = -1, saw = Integer.MIN_VALUE;
    for (int i = 0; i < iters; i++) {
      int h = Statics.anyHash(v);                 // must be -2 for every i
      if (h != -2) { bad++; if (first < 0) { first = i; saw = h; } }
    }
    System.out.println("bad=" + bad + " first=" + first + " saw=" + saw);
  }
}
```

`scala.runtime.Statics.anyHash(Object)` is three branches of bytecode:

```text
 0: aload_0 / ifnonnull 6 / iconst_0 / ireturn        // null -> 0
 6: aload_0 / instanceof java/lang/Number / ifeq 21
13: checkcast java/lang/Number / invokestatic anyHashNumber:(Ljava/lang/Number;)I / ireturn
21: aload_0 / invokevirtual java/lang/Object.hashCode:()I / ireturn
```

and `anyHashNumber` is the `Long`/`Double`/`Float` `instanceof` chain that ends
in `longHash(lv)` — `int iv = (int) lv; iv == lv ? iv : Long.hashCode(lv)`,
which for every value in `int` range is just the value. `anyHash(-2L)` is `-2`
on every conforming JVM.

## Measurement

3,000,000 iterations per run, `--XX:UseGc` unset (default), Azure Linux
(`20.80.105.49`), `dev`@`39a90d2f4` plus the unrelated generics fix:

| arm | runs | result |
|---|---|---|
| stock HotSpot 25 | 1 | `bad=0` |
| CratonVM `--nojit` | 1 | `bad=0` |
| CratonVM, JIT on | 20 | **12 runs `bad≈2,999,300`** (wrong from `i≈500-950` onward, forever) · **6 runs `bad=236-980`** (a bounded window, then correct) · **2 runs `bad=0`** |

So: JIT-only, and **nondeterministic across otherwise identical runs** — the
same binary, same arguments, same host, sometimes clean, usually catastrophic.

What the wrong answer looks like matters:

* It is a **per-run constant**. Three distinct `Long` objects with three
  distinct values (`Long.valueOf(-2)`, `new Long(-2)`, `Long.valueOf(999999)`)
  all return the SAME wrong int in a run (`9588736` in one, `16777237` in
  another, `0` in a third). The argument is not reaching the arithmetic at all.
* `0` — the `x == null` arm — is one of the values it takes.
* `Double`, `Float`, `Integer` and `String` receivers are **correct in the same
  loop**. Only the `Long` branch is wrong.
* `Statics.longHash(long)` called DIRECTLY is always correct, and a
  line-for-line Java replica of `anyHash`/`anyHashNumber`/`longHash` in an
  ordinary class is always correct. The defect needs the real class.

## What is localized, and what is not

`CRATONVM_JIT_DENY` (substring match on `Class.method`, force-interpret) run at
3M iterations:

| deny | result |
|---|---|
| `scala/runtime/Statics.anyHash` | clean |
| `scala/runtime/Statics.anyHashNumber` | clean |
| `scala/runtime/Statics.longHash` | clean |
| `scala/runtime/Statics` (whole class) | clean |
| `AnyHashOnly.main` (the CALLER) | still fails |
| `java/lang/Long.longValue`, `java/lang/Long.hashCode`, `java/lang/Integer.bitCount`, `scala/collection/immutable/…` | still fails |

So the wrong body is the compile of `Statics.anyHash` with `anyHashNumber` and
`longHash` spliced into it, and denying any one of the three splices removes
it. The caller is innocent.

`CRATONVM_DBG=jit-slot-overlap` fires on exactly those three methods:

```text
[jit-slot-overlap] reservation 72..80 (Push) overlaps OPEN inline scope #0/1 INNERMOST
  (the splice that is returning; its locals are dead here) locals 72..88 (num_locals=2)
  in scala/runtime/Statics.longHash:(J)I
```

`open_inline_locals_floor` deliberately excludes the INNERMOST scope, on the
grounds that it is "the splice that is RETURNING: its locals are dead at that
instruction". With exactly one open scope that exclusion removes the floor
entirely. Whether this report is the defect or the benign return-reclaim it was
written to tolerate is NOT established here — `doubleHash` and `floatHash` draw
the identical report and answer correctly.

### A dead end, recorded so it is not walked twice

A single-run sweep of eighteen `CRATONVM_JIT_*` feature flags appeared to
isolate `CRATONVM_JIT_CALL_SPILL_ELISION` (default `3`, the MIC/PIC arm):
`=0` read `bad=852` against `bad=799,018` for the default, a 940x drop, and
modes 0/1/2 all read like the fix.

**It did not survive repetition.** Five runs per mode:

| mode | five runs |
|---|---|
| 0 | 618 · 980 · 2999498 · 2999498 · 2999498 |
| 1 | 572 · 2999498 · 2999065 · 368 · 2999498 |
| 2 | 2999279 · 2999383 · 0 · 2999498 · 301 |
| 3 (default) | 0 · 623 · 0 · 658 · 236 |

Every mode spans the same range. The flag does nothing here; the sweep was
reading run-to-run variance, and a one-run-per-arm sweep over a bimodal
nondeterministic vector cannot say otherwise. Measure this one with at least
five runs per arm.

## Workaround

`CRATONVM_JIT_DENY=scala/runtime/Statics` makes the Kafka class pass and costs
nothing outside Scala code. It is a diagnostic lever, not a shipping fix.

`--nojit` also passes (2/2) and is not an option for the suite.

## Next step for whoever picks this up

The compiled body is available — `CRATONVM_DBG=jit-disasm` with
`CRATONVM_DBG_JIT_DISASM=scala/runtime/Statics.anyHashNumber` dumps 9,240 bytes
of `full/sp` code for a method whose bytecode is 59 bytes. Read the `longHash`
splice's argument load against the `Push` reservation the overlap diagnostic
names, and check whether the frame slot `iv` (callee local 2 — `longHash`
declares `locals=3`, and the reported scope carries `num_locals=2`) is inside
the reserved range.

Related: `docs/internal/fixed-bugs/inline-splice-return-value-lands-on-an-enclosing-callees-live-local-FIXED-20260907.md`
is the same family and the source of `open_inline_locals_floor`.
