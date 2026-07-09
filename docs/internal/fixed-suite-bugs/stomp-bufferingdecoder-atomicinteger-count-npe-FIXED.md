# STOMP `BufferingStompDecoder` NPE — `AtomicInteger` field "count" null (FIXED)

| | |
|---|---|
| **Status** | ✅ FIXED 2026-07-07, commit `4f32fb6b` (merged `86f37f84`). |
| **Area** | Native `LinkedBlockingQueue.clear()` — synthetic-layout assumption corrupted the real-JDK field slot. |
| **Discovered** | 2026-07-07, triaging FAIL results from a 516-class Spring non-passed rerun on Azure (dev, real-JDK, JIT on). |

## Symptom

```
java.lang.NullPointerException: Cannot invoke "java.util.concurrent.atomic.AtomicInteger.get()" because "count" is null
```

`org.springframework.messaging.simp.stomp.BufferingStompDecoderTests` failed
identically on 5 methods: `incompleteCommand()`,
`oneFullAndOneSplitWithContentLengthExceedingBufferSize()`,
`oneFullAndOneSplitMessageContentLength()`, `oneMessageInTwoChunks()`,
`oneFullAndOneSplitMessageNoContentLength()`. HotSpot passed all 5.

## Root cause

Not a field-initializer ordering bug as initially hypothesized. The real
cause: `LinkedBlockingQueue.clear()`'s real-JDK-mode native override
(`register_essential_natives`) unconditionally wrote `Value::Int(0)` into
field slot 1, assuming the synthetic 4-field stub layout
(array/size/capacity/...). On a real-JDK-constructed `LinkedBlockingQueue`,
slot 1 is actually the real `count` field — an `AtomicInteger` — so `clear()`
stomped it with a bare `Int(0)`, corrupting the field to a non-`AtomicInteger`
value that later reads as null through the `AtomicInteger`-typed field
accessor.

## Fix

Commit `4f32fb6b` — `LinkedBlockingQueue.clear()`'s native no longer
unconditionally writes the synthetic-layout slot; it now checks whether the
receiver is a real-JDK `LinkedBlockingQueue` (real `count` field is an
`AtomicInteger`, not a bare int) and clears it correctly for that layout
instead of stomping it. Regression coverage added in `c9b25de2`
("Add STOMP queue count regression coverage"). Merged to dev via `86f37f84`.

## Reproduction (pre-fix)

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
```

Post-fix rerun on current dev confirms `BufferingStompDecoderTests` 11/11 OK.
