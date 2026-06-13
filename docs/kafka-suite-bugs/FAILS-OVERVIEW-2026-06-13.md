# CratonVM-only FAIL classes — catalog & bugfix handoff (2026-06-13)

69 classes are **FAIL on CratonVM but OK on HotSpot** (wrong results, not crashes;
crashes/hangs are bug-21/22/23). Data from the clean pre-merge `cv-full2` run; re-check
against `9d8bba97` (the dev merge `b5c709c7` touched collections/JCA/JIT and may have
already reduced several). Grouped by root-cause cluster, largest first.

| Cluster | # cls | Doc | Classes |
|---------|------:|-----|---------|
| **Generated-message array null hole** (`cacheSerializedValue/tags/topics/errorCode on null`) | 18 | [bug-25](bug-25-message-array-null-hole-cacheSerializedValue.md) | AlterPartitionRequest, CreateAcls/DeleteAcls(Req+Resp), DescribeAcls(Req+Resp), DeleteTopicsRequest, OffsetCommitResponse, StopReplicaRequest, TxnOffsetCommitResponse, UpdateFeaturesRequest, MetadataRequest, LeaveGroupResponse, RequestResponse, SimpleExampleMessage, NullableStructMessage, Frequencies, TopicMetadataFetcher |
| **SCRAM `HmacSHA256` Mac unavailable** | 4 | [bug-26](bug-26-scram-hmacsha-mac-unavailable.md) | ScramCredentialUtils, ScramFormatter, ScramMessages, ScramSaslServer |
| **File-record `seek before start` + LZ4 native missing** | 3 | [bug-27](bug-27-filechannel-seek-before-start.md) | FileLogInputStream, RemoteLogInputStream, UnalignedFileRecords (+ CompressionType: `NoClassDefFoundError net/jpountz/lz4/LZ4JNI`) |
| **JIT dispatch `InternalError` (MIC/PIC)** | 2 | [bug-24](bug-24-jit-mockito-mic-pic-crash.md) (other session) | CommonNameLoggingSslEngineFactory, DefaultSslEngineFactory |
| **`Object → Locale` ClassCastException** | 2 | _below_ | GarbageCollectedMemoryPool, KafkaLZ4 |
| **`LoginContext` state lost post-GC** | 2 | _below_ | LoginManager, JaasContext |
| **Metric double-registration** | 1 | _below_ | CompletedFetch |
| **`@ParameterizedTest` empty stream** (bug-14 family) | 1 | bug-14 | FetchCollector |
| **Other wrong-result assertions** (long tail) | 36 | see below | see below |

## Smaller clusters (no separate doc yet)

### `java.lang.ClassCastException: java/lang/Object cannot be cast to java/util/Locale` (2)
`GarbageCollectedMemoryPoolTest`, `KafkaLZ4Test`. A `Locale`-typed argument slot
receives a bare `Object` — likely a `String.format(Locale, …)` / `toLowerCase(Locale)`
varargs or intrinsic passing the wrong slot. Find the `Locale` call site in each test's
path and check the CratonVM `String.format`/`Formatter` intrinsic argument handling.

### `IllegalStateException: LoginContext state missing post-GC or never initialized` (2)
`LoginManagerTest`, `JaasContextTest`. A `javax.security.auth.login.LoginContext`'s
native/cached state is lost after a GC — same **GC-root/stale-ref family** as bug-22
(a cached reference not scanned/remapped). Cross-ref `reference_classloader_gc_root_gap`.

### `IllegalArgumentException: A metric named '…' already exists` (1)
`CompletedFetchTest`. A metric is registered twice — a CratonVM `Map`/`Set` containsKey
or `putIfAbsent` returning the wrong answer (dup not detected), or a teardown that
didn't remove. Check the metrics registry collection intrinsic.

## Other-assertion long tail (36) — likely maps to existing bug docs
Assignors (`RangeAssignor` 16/44, `RoundRobinAssignor`, `ConsumerPartitionAssignor`,
`SubscriptionState`) → **bug-17** (map/set ordering). `UuidTest` 61/108 → **bug-18**
(Uuid). Iterators (`FlattenedIterator` 2/6, `MappedIterator` 1/2,
`ImplicitLinkedHashCollection/MultiCollection`) → collection-intrinsic ordering/holes
(**bug-12/16** family). Config (`AbstractConfig`, `ConfigDef`), OAuth bearer
(`OAuthBearer*`, `ValidatorAccessTokenValidator`, `AccessTokenRetrieverFactory`),
records (`AbstractLegacyRecordBatch`, `LazyDownConversionRecords`, `ControlRecordUtils`),
checksums (`Checksums` 2/4, `Bytes`), and the admin handlers
(`ListOffsetsHandler`, `ListTransactionsHandler`, `AllBrokersStrategy`,
`FetchSessionHandler`) are individual wrong-result bugs — triage each by its first
`AssertionFailedError expected/actual` and attach to an existing or new cluster.

## Recommended fix order (impact-weighted)
1. **bug-25** (18 classes) — one collection null-hole fix likely clears the whole ACL/
   request/response cluster.
2. **bug-26** (4, all 0-pass) — register `HmacSHA256/512` Mac SPI; small, self-contained.
3. **bug-17/bug-18** (assignor ordering / Uuid) — clears a chunk of the long tail.
4. **bug-27** (file seek) + **bug-22-family** GC stale-ref (`LoginContext`).
