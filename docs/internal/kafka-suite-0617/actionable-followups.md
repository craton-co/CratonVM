# Kafka suite 0617 — actionable follow-up bugs (post Bug A/C/D)

Ranked, ready-to-implement diagnoses for the remaining CratonVM-only failures (root causes,
not per-class symptoms). Build note: `native-builtins/src/lib.rs` is huge → build with
`RUST_MIN_STACK=128M` to avoid the rustc `STATUS_STACK_BUFFER_OVERRUN` at the `lto=fat` link.

## 1. Missing Snappy/Zstd native compression (12 classes — ONE root cause) ⭐ top pick

`UnsatisfiedLinkError` on third-party compression JNI:
- 9× `org/xerial/snappy/SnappyNative.maxCompressedLength(I)I` (+ rest of the Snappy JNI)
- 3× `com/github/luben/zstd/ZstdOutputStreamNoFinalizer.recommend…` (+ zstd-jni JNI)

Affected: `SnappyCompressionTest`, `ZstdCompressionTest`, `ProducerBatchTest`,
`DefaultRecordBatchTest`, `FileLogInputStreamTest`, `RemoteLogInputStreamTest`,
`ProduceRequestTest`, `PushTelemetryRequestTest`, `ClientTelemetryUtilsTest`,
`BatchBuilderTest`, `RecordsBatchReaderTest`, `LogValidatorTest`.

**Fix:** register Rust-native shims for the snappy-java + zstd-jni JNI entrypoints (the codebase
already ships gzip via `flate2`; add the `snap` + `zstd` crates and wire `SnappyNative.*` /
`Zstd*` like LZ4/GZIP). One feature unblocks all 12 — highest impact-per-fix.

## 2. TreeMap / NavigableMap `*Entry` methods missing

`NoSuchMethodError: cratonvm/internal/UnmodifiableMap.higherEntry(...)`. Underlying gap: TreeMap
has `higherKey/lowerKey/floorKey/ceilingKey` + `first/lastEntry`, but **not** the relative
`*Entry` variants. **Two TreeMap impls** must both get them:
- `native-collections/src/lib.rs` `native_tm_*` (array + fast-mode BTreeMap) — mirror
  `native_tm_higher_key` returning `tm_make_entry(key,value)`; register on `TreeMap` + `SortedMap`.
- `native-builtins/src/phases_late.rs` `p62_tm_*` (linear scan, field0=data field1=size) — mirror
  `p62_tm_higher_key`; register on `TreeMap` + `NavigableMap`.
- Add `UnmodifiableMap` delegations for the 4 `*Entry` (`unmod_delegate`).

## 3. IntStream/LongStream.reduce not implemented (AbstractMethodError "no Code attribute")

`IntStream.reduce(I,IntBinaryOperator)I`, `LongStream.reduce(LongBinaryOperator)OptionalLong` —
add the reduce terminals to the synthetic primitive streams.

## 4. MessageDigestSpi.engineDigest() "no Code attribute"

A digest provider path reaches abstract `MessageDigestSpi.engineDigest()[B` with no impl.
Identify the algorithm and back it (or route to the real SUN provider, like ML-DSA Signature).

## 5. AtomicLongFieldUpdater$RustJvmImpl.getAndIncrement(Object)J missing

`NoSuchMethodError`. **Coordinate** with the active `fix/atomic-field-updater-methods` branch
(add `getAndIncrement`/`getAndAdd`/etc. there).

## 6. NullPointerException cluster (62) — per-class triage

Largest FAIL bucket but heterogeneous; re-run on latest dev with the enhanced KRun (full
traces) and group by origin frame. See [cluster-npe-heterogeneous.md](cluster-npe-heterogeneous.md).

## Excluded (not VM bugs)
- Classes failing on **both** HS and CratonVM are real Kafka test/env issues — except the broker-
  dependent subset (see [bug-F](bug-F-broker-integration-gaps.md)), which ARE CratonVM gaps once a
  broker is available.
- `cratonvm/Util.tempPrint`-style `UnsatisfiedLinkError` fixtures are a harness artifact (need
  `--features synthetic-jdk`), not VM bugs.
