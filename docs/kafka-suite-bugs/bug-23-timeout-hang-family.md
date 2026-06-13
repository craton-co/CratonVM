# Bug 23 — TIMEOUT family (30 classes): mostly throughput, not deadlocks

**Severity:** Mixed. 30 classes hit the 90 s per-class wall on CratonVM that HotSpot
clears (HotSpot's only timeout is one DNS-dependent class). Classifying them by
watchdog stack-dump **leaf frame** (where the thread actually spends time) shows the
overwhelming majority are **slow, not hung** — CratonVM's interpreter/GC throughput
(~8×) amplified by reflection-heavy JUnit discovery and Mockito/ByteBuddy mock
generation. Only a couple show a genuine `Object.wait` blocking frame.

## How they were classified

Run each class with the default 120 s watchdog **enabled** (it dumps all thread
stacks before aborting) and look at the deepest frame across dumps. A frame that
*advances* between dumps (regex, hashing, reflection, mock-gen) = slow; a frame
pinned on `park`/`Object.wait`/`Condition.await` = candidate true hang.

```
cd apps/kafka/tests
CP=".;$(cat cp.txt)"
timeout -k5 135 ./kv2.exe -cp "$CP" KRun <fqcn>   # watchdog ON, dumps stacks at 120s
```

## Findings by leaf frame (representative)

| Class | Leaf frame(s) | Verdict |
|-------|---------------|---------|
| `common.utils.SanitizerTest` (2 tests!) | `ObjectName.quote`→`Pattern.matcher`(regex)→`hashCode` | **slow** — JMX register/unregister loop, regex throughput |
| `admin.KafkaAdminClientTest` (182) | `ReflectionUtils.toSortedMutableList`, `DisplayNameUtils` | **slow** — JUnit reflective discovery |
| `consumer.KafkaConsumerTest` | `ReflectionUtils.toSortedMutableList`, `AnnotationUtils.findAnnotation` | **slow** — discovery |
| `consumer.internals.AsyncKafkaConsumerTest` | `net.bytebuddy…TypeDescription.<init>`, `asm.ClassReader.readUtf` | **slow** — Mockito/ByteBuddy mock generation |
| `consumer.internals.ConsumerNetworkClientTest` | `net.bytebuddy…asErasure` | **slow** — ByteBuddy |
| `consumer.internals.CommitRequestManagerTest` | `InlineDelegateByteBuddyMockMaker`, `NamespacedHierarchicalStore$CompositeKey.hashCode` | **slow** — Mockito |
| `clients.NetworkClientTest` | `StackStreamFactory$StackFrameBuffer.fill`, `ClassFrameInfo.<init>` | **slow** — exception/stack-trace capture heavy |
| `consumer.{Cooperative,}StickyAssignorTest` | `AbstractStickyAssignor$RackInfo.lambda$new$8` | slow / assignor-heavy (see also bug-17) |
| `consumer.internals.AbstractCoordinatorTest` | **`java/lang/Object.wait`** + `ApiVersionsResponseData$ApiVersion.<init>` | **candidate true hang** — a thread blocked in `Object.wait`; needs a lost-wakeup check |
| `common.record.LegacyRecordTest` (1440), `MemoryRecordsTest` (379), `MemoryRecordsBuilderTest` (593) | parameterized bulk | **slow** — huge parameterized count × interpreter |

### Full 30-class TIMEOUT list (HotSpot status in parens)
HS=OK (11): `SanitizerTest`, `MetricsTest`, `StickyAssignorTest`,
`CooperativeStickyAssignorTest`, `TransactionManagerTest`, `KafkaAdminClientTest`,
`AbstractCoordinatorTest`, `OffsetFetcherTest`, `LegacyRecordTest`, `MemoryRecordsTest`,
`MemoryRecordsBuilderTest`.
HS=FAIL (15): `BufferPoolTest`, `CooperativeConsumerCoordinatorTest`,
`EagerConsumerCoordinatorTest`, `HttpAccessTokenRetrieverTest`,
`ConsumerNetworkClientTest`, `FileRecordsTest`, `OffsetsRequestManagerTest`,
`SelectorTest`, `RecordAccumulatorTest`, `NetworkClientTest`, `DefaultRecordBatchTest`,
`CommitRequestManagerTest`, `SenderTest`, `FetchRequestManagerTest`, `FetcherTest`.
HS=LOADERR (4): `KafkaConsumerTest`, `AsyncKafkaConsumerTest`, `KafkaProducerTest`,
`SaslServerAuthenticatorTest`.

## Recommendation

- These are **not** distinct crashes; do not file one bug each. The dominant cost is
  (a) Mockito/ByteBuddy mock generation and (b) JUnit reflective discovery under the
  interpreter — i.e. the known throughput gap, not correctness defects.
- The one item worth a focused look is **`AbstractCoordinatorTest`** (`Object.wait`
  leaf): confirm it is a lost-wakeup deadlock vs just slow by running it with a much
  larger timeout — if it completes given time, it is slow; if it never completes, it
  joins the `BufferPool`/AQS blocking-primitive family (cf. bug-19, `CRATONVM_REAL_AQS`).
- A throughput win on the Mockito mock-generation path (ByteBuddy `TypeDescription`
  construction) would clear the largest cluster of these timeouts.

> Note: precise hang-vs-slow re-runs at 300 s were started but cut short by the
> concurrent-session CPU contention documented in `RUN-2026-06-13-summary.md`; the
> leaf-frame verdicts above come from the 120 s watchdog dumps, which are valid
> regardless of contention.
