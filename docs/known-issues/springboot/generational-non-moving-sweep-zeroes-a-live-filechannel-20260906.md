# The Generational non-moving young sweep zeroes a LIVE `FileChannelImpl`

| | |
|---|---|
| **Status** | PARTIALLY FIXED 2026-09-06. The RECLAIM's producer is found and fixed — `native_fcimpl_open` built the channel out of five allocations while holding every result in an unrooted Rust local, so nothing referred to them until the first field store; see `internal/fixed-bugs/filechannelimpl-construction-window-was-never-rooted-FIXED-20260906.md`. The test's remaining failure is NOT explained by that and stays OPEN — it fails at the same rate with the reclaim gone. |
| **Was** | OPEN. Reproducible on dev `785e777cb`, named by a one-flag probe, 4 reclaim lines in each of two instrumented runs. |
| **Scope** | `--XX:UseGc Generational` with the JIT on. HotSpot, CratonVM ZGC (the shipped default) and Generational `--nojit` all pass the same test at the same host load. |
| **Reproducer** | one test METHOD, 1-10 minutes |
| **Probe** | `CRATONVM_DBG_SWEEP_ZERO=1` names the victim by class and sweep cycle |
| **Supersedes** | the mechanism half of the retired `moving-young-jit-frame-fallback-costs-3-10x-20260906` page, which attributed this test's failure to `[moving-young]` fallbacks. It is not that — see "What this is not". |

## The defect

```text
[sweep-zero] RECLAIMED-LIVE receiver ptr=0x77d695f6a090:
  original class=sun/nio/ch/NativeThreadSet (class_id=3299 kind=0x00),
  zeroed by non-moving sweep cycle 22; invoked as sun/nio/ch/NativeThreadSet.add
  — the live ref was a register/native-stack root the marker missed
```

A live object is zeroed by the young sweep and then invoked. Across runs the
victims are `sun/nio/ch/NativeThreadSet` (the set a `FileChannelImpl` keeps its
blocking-I/O threads in), `sun/nio/ch/FileChannelImpl` itself, and a class
mirror — 3, 11 and 4 reclaim lines in three instrumented runs.

## What it costs

The reproducer is
`org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests#testEndToEndWithRetryTopics`,
which starts an `@EmbeddedKafka` KRaft broker. A zeroed `FileChannelImpl` reads
its `fd` field back as 0, so:

```text
java.io.IOException: FileChannel.map: invalid fd
  at AbstractIndex.createMappedBuffer                 (the broker's index mmap)
→ UnknownTopicOrPartitionException: This server does not host this topic-partition
→ TimeoutException: Topic testRetryTopic not present in metadata after 60000 ms
→ org.apache.kafka.common.KafkaException: Send failed
```

The broker never publishes its topics, so the test cannot send.

## Repro

```bash
# azureuser@20.80.105.49 — run it ALONE, it binds a broker port
cd /data/cratonvm/apps/spring-boot/module/spring-boot-kafka
CP="../../sb-runner:$(tr '\n' ':' < build/cratonvm-test-cp.txt)"
CRATONVM_DBG_SWEEP_ZERO=1 $CVM --java-home $JDK --Xmx 2g \
     --add-opens=java.base/java.net=ALL-UNNAMED \
     --stack-dump-on-timeout 0 --XX:UseGc Generational \
     -cp "$CP" SbRunnerMethod \
     org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests \
     testEndToEndWithRetryTopics
# then: grep -c RECLAIMED-LIVE, and grep -o 'original class=[^ ]*'
```

## Arms — with SAME-TIME controls

Round-robin, one process at a time, so a drift in host load lands on every arm
equally. The controls are not a VM comparison: they are what separates "the
collector is broken" from "the box is too loaded for a broker to start", which
this test is extremely sensitive to. An uncontrolled first pass had the
`--nojit` arm failing once at 414 s and passing at 80 s.

| arm | result | wall |
|---|---|---|
| HotSpot `jdk-25` | PASS | 14, 16, 17, 20 s |
| CratonVM **ZGC** (shipped default) | PASS | 57, 60 s |
| CratonVM **Generational** `--nojit` | PASS | 68, 80 s |
| CratonVM **Generational** | **FAIL** | 108, 125, 164, 194, 302, 329, 586 s |
| Generational `CRATONVM_NO_MOVING_YOUNG=1` | FAIL 2, PASS 1 | 87, 140, 195 s |

## What this is not

**Not the `[moving-young]` fallback.** `CRATONVM_NO_MOVING_YOUNG=1` holds the
young collector permanently in exactly the state a fallback puts it in —
non-moving sweep, free-list allocation, and no probe to produce the warning —
and the test fails there too, with ZERO fallback warnings. The retired page
attributed the failure to those fallbacks and classified it as throughput; the
non-moving sweep is not the collector "declining to move", it is where the live
object is destroyed.

**Not the coverage probe's residue.** The `unregistered-jit-frame-on-stack`
refusals that page named are 100% residue and are now screened out at the
coverage probe (`CRATONVM_JIT_A5_RESIDUE_FILTER`). That fix does not change this
failure, which is the point of filing it separately.

## The lead

`scan_active_jit_frames`'s residue filter decides whether to conservatively MARK
the band above the JIT entry chain. Standing it down makes the site mark more:

| arm | reclaimed-live lines | test | wall |
|---|---|---|---|
| Generational, default | 4, 4 | FAIL, FAIL | 194 s, 900 s (cap) |
| `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` | **0, 0** | FAIL, FAIL | 146 s, 140 s |
| `CRATONVM_JIT_NO_RETPC_VALIDATE=1` | 6 | FAIL | 715 s |

Both reps agree: standing that screen down takes the reclaim from 4 to 0, and
leaves it there. So it IS the producer of the premature reclaim — and the
reclaim is NOT sufficient for the test failure, which happens anyway with zero
reclaims. The other screen on the same site, return-PC validation, is not
implicated (it marks LESS, and the reclaim count goes up).

Two things follow. The reclaim is real and worth fixing on its own; and there is
a second cause of the test failure that none of these arms has touched.

Not attempted here: a per-victim provenance for the missed root (which band,
which frame, whether the holder was a JIT register or a native local), and a
`CRATONVM_DBG_A5_FALLBACK=1` correlation between the residue hits the screen
suppresses and the addresses the sweep later zeroes — the two should name the
same band if the screen really is the whole producer.
