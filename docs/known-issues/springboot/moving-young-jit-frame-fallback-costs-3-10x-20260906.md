# The `[moving-young]` JIT-frame fallback is back, and it costs 3–10x on a Kafka end-to-end test

| | |
|---|---|
| **Status** | OPEN. Reproducible on dev `0cd363b64`, with the mechanism established by ablation rather than inferred. **Not a correctness defect** — the fallback is the collector declining to move, which is the sound direction. It is a throughput defect, and on a test carrying its own timeout it presents as a failure. |
| **Scope** | `--XX:UseGc Generational` only. ZGC — the shipped default — is unaffected (0 fallbacks, passes). G1 is unaffected by *this* mechanism (0 fallbacks); its failures on the same test are a different defect, see "What this is not". |
| **Reproducer** | one test METHOD, 1–10 minutes depending on which side of the cliff the run lands. |

## The measurement

`org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests#testEndToEndWithRetryTopics`
starts an `@EmbeddedKafka` broker, publishes one record through a retry-topic
chain, and waits on `listener.latch.await(30, TimeUnit.SECONDS)`. The assertion
that fails is that `await` returning false: the round trip did not finish inside
the test's own 30-second budget.

One method per process, `-Parallel 1`, on `azureuser@20.80.105.49` at load 15–22
throughout — so read the rows against each other, not as absolute numbers. The
HotSpot and ZGC rows are what make the comparison legitimate: they ran on the
same loaded box.

| VM / collector | wall (s) | result | `[moving-young]` fallback warnings |
|---|---|---|---|
| HotSpot `jdk-25.0.4+7` | 19, 19 | PASS | — |
| CratonVM **ZGC** (default) | 48 · 64, 47, 56 | PASS | 0 |
| CratonVM **Generational** | 600†, 206 · 136, 601†, 601† | TIMEOUT / FAIL | **13–14 warnings, counter past #64** |
| CratonVM **Generational `--nojit`** | 62 · 68, 63 | **PASS** | **0** |

† the 600 s harness cap, not a completion. Where two groups of numbers appear,
the first is dev `0cd363b64` and the second the earlier `8d83c7585` build; the
result does not differ between them.

## The mechanism, by ablation

The generational arm's stderr carries the warning this page is named for, at a
count that keeps doubling (the warning itself is rate-limited to powers of two,
so 13–14 lines is a counter past #64):

```text
WARN cratonvm_gc::gc_quiescence: [moving-young] fallback #64:
  reason=unregistered-jit-frame-on-stack — a live JIT frame could not prove a
  complete rewritable root map, so this young collection runs the NON-MOVING
  sweep (no compaction, free-list allocation). Persistent fallbacks mean the
  young generation is not actually a copying collector.
```

`--nojit` removes the trigger — no compiled frames, nothing whose root map
cannot be proved — and with it **both** effects: the warnings go 13 → 0 and the
600-second timeout becomes a 62-second pass. One variable, both outcomes, twice
on each binary.

Note what `--nojit` does not do. It does not make the collector correct; it
removes the frames whose root map the JIT cannot prove. The gap is the JIT's,
and the young collector's refusal to move is the sound response to it — which is
why this is filed as throughput and not as corruption.

## This is the fourth appearance of this mechanism on this suite

The retired `moving-young-fallback-four-springboot-classes` write-up closed the
same warning on four Spring Boot classes on 2026-08-18, on the measurement that
its peaks had fallen from #2048 to #1–#5 — by that page's own triage rule,
"single digits → harmless, thousands → death spiral". This run's counter passes
**#64** on one test method, which puts it between the two, and that page
predicted its own return in those words: *"the fix might just move the failure."*

What is new is the reading. That page measured whole classes and treated the
PEAK as the signal. The peak is not the signal — **the cost is**. Sixty-four
fallbacks over a 136-second method is not a spiral, and it is still a 3–10x
throughput loss, because every one of them is a young collection that compacted
nothing and allocated from a free list. A test with a latch is simply the first
thing to notice.

## What this is not

The same test method also fails under G1, and folding that in here would be
wrong. G1 logs **zero** `[moving-young]` fallbacks on it, and
`CRATONVM_G1_PARALLEL_EVAC=0` turns 410 s/FAIL into 94 s/PASS — that is the G1
parallel-evacuator defect, filed as
`g1-parallel-evacuator-corrupts-live-references-20260906.md`.
Two collectors, two defects, one test method; the only thing they share is that
both need compiled frames on the stack.

## Repro

```bash
# azureuser@20.80.105.49, release binary — run it ALONE, it binds a broker port
cd /data/cratonvm/apps/spring-boot/module/spring-boot-kafka
CP="../../sb-runner:$(tr '\n' ':' < build/cratonvm-test-cp.txt)"
$CVM --java-home $JDK --Xmx 2g --add-opens=java.base/java.net=ALL-UNNAMED \
     --stack-dump-on-timeout 0 --XX:UseGc Generational \
     -cp "$CP" SbRunnerMethod \
     org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests \
     testEndToEndWithRetryTopics
# then the same line with --nojit appended; diff the wall time and
# `grep -c 'moving-young. fallback'` over stderr
```

A concurrent copy of the class, or a build sharing the box, turns both arms into
hangs that say nothing — which is how this was mis-recorded as "generational
only, one run in two" in the first place.
