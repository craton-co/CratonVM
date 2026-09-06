# The Generational non-moving young sweep zeroes a LIVE `FileChannelImpl`

| | |
|---|---|
| **Status** | OPEN. Reproducible on dev `785e777cb`, named by a one-flag probe, 4 reclaim lines in each of two instrumented runs. |
| **Scope** | `--XX:UseGc Generational` with the JIT on. HotSpot, CratonVM ZGC (the shipped default) and Generational `--nojit` all pass the same test at the same host load. |
| **Reproducer** | one test METHOD, 1-10 minutes |
| **Probe** | `CRATONVM_DBG_SWEEP_ZERO=1` names the victim by class and sweep cycle |
| **Supersedes** | the mechanism half of the retired `moving-young-jit-frame-fallback-costs-3-10x-20260906` page, which attributed this test's failure to `[moving-young]` fallbacks. It is not that — see "What this is not". |

## A mechanism to test first, added 2026-09-06

A defect with THIS PAGE'S EXACT SCOPE — Generational with the JIT on; HotSpot,
ZGC, G1 and Generational `--nojit` all clean — was root-caused the same day, and
it is not a collector bug at all:

> a native that re-enters Java holds its argument snapshot across the
> collection that re-entry can trigger. `safe_native_call_impl` pins every
> argument, so nothing is collected, but it rebuilds the snapshot from those
> pins only for a collection it runs ITSELF, before the callback. A young
> collection inside the callback relocates the object — Cheney copy, or
> selective promotion even on the non-moving path — and the snapshot keeps
> naming the old address.

The read through that stale address finds a zeroed header and decodes as
`ClassId(0)` / `java.lang.Object`, which is exactly the signature this page
reports.

Why it is worth testing here before more collector work: this page's victims —
`sun/nio/ch/NativeThreadSet`, `sun/nio/ch/FileChannelImpl` — are precisely the
objects an `sun/nio/ch` native holds across a re-entrant call, and its
conclusion that "the live ref was a register/native-stack root the marker
missed" is what a stale Rust-side snapshot looks like from the marker's side.

It is a DIFFERENT native, so the 2026-09-06 fix does not touch this test. The
reproducer it came with is the useful part: `GpuResidencyGc 0 1024 800` under
`-XX:+UseGenerationalGC`, ~20 seconds, no GPU and no Azure. Calibrate against
that before spending another Kafka broker start.

Full write-up: `native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md`.

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
