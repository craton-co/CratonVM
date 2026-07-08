# STOMP `BufferingStompDecoder` NPE — `AtomicInteger` field "count" null

| | |
|---|---|
| **Status** | OPEN, found 2026-07-07 during a Spring suite non-passed rerun on Azure (dev, real-JDK, JIT on). |
| **Area** | Instance-field initialization — an `AtomicInteger` field is null when a chunked-decode method reads it. |

## Symptom

```
java.lang.NullPointerException: Cannot invoke "java.util.concurrent.atomic.AtomicInteger.get()" because "count" is null
```

`org.springframework.messaging.simp.stomp.BufferingStompDecoderTests` fails
identically on 5 methods: `incompleteCommand()`,
`oneFullAndOneSplitWithContentLengthExceedingBufferSize()`,
`oneFullAndOneSplitMessageContentLength()`, `oneMessageInTwoChunks()`,
`oneFullAndOneSplitMessageNoContentLength()` — one shared root cause.

A field named `count` (type `AtomicInteger`, tracking a decode/chunk count in
`BufferingStompDecoder`) is unexpectedly null when these multi-invocation /
chunked-decode paths run. HotSpot passes all 5.

## Initial read

Likely an instance-field-initializer ordering/visibility bug: if `count` is
assigned via an inline field initializer (`new AtomicInteger(...)` at the
field declaration, not in an explicit constructor body — a common Spring
code style), CratonVM may be executing/observing that initializer out of
order relative to when the decode methods run, or losing the assignment
across some call boundary. Possibly related to the "stale-local" /
persistent-singleton root family already tracked (see
`native-stale-local-family-and-persistent-singleton-roots` history), but
should be verified as a distinct instance since this is a plain instance
field, not a singleton/static.

Read `BufferingStompDecoder` in `spring-messaging` to find the exact `count`
field declaration and where it's read to confirm the exact code shape.

## Reproduction

Azure host suite runner (path depths on this host shift between `/data/data/`
and `/data/data/data/` — verify with `ls -d` at both before trusting either):

```bash
WT=/data/data/wt-osr-nonpassed-20260706-1945   # prebuilt Spring suite + frozen binary
cd $WT/apps/spring-suite-runner
echo org.springframework.messaging.simp.stomp.BufferingStompDecoderTests > /tmp/list.txt
SF=$WT/apps/spring-framework RUNNER=$WT/apps/spring-suite-runner \
  CRATONVM_BIN=$WT/cratonvm-osr-nonpassed-20260706.bin JH=/data/data/jdk25-real \
  BATCH=1 BATCH_TO=120 ONE_TO=120 LIST=/tmp/list.txt OUT=/tmp/out SHARD_N=1 SHARD_ID=0 \
  bash suite-run.sh
# see /tmp/out/failcauses.log and /tmp/out/raw.log
```
