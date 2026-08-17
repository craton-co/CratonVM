# hibernate-reactive — investigate batch 01 of 6 — CLOSED

> **CLOSED 2026-08-12.** All 12 classes were re-run per class, per GC variant,
> against the dev tip (`ad7b496ef`) with a HotSpot JDK 25 control on the same
> classpath. **11 of the 12 were already green** — they were collected behind
> the SASL/SCRAM and JNA blockers, exactly as the stale banner this page used to
> carry predicted. The twelfth, `BatchingConnectionTest`, hid a real CratonVM
> defect behind them: a young-GC livelock under `-XX:+UseGenerationalGC`, now
> fixed. See "Result" below. Batches 02-06 are untouched and still owe the same
> per-class check.

Part of a 71-class FAIL/HANG list split across 6 pages (see
[investigate-INDEX.md](investigate-INDEX.md)). This page owns exactly the 12
classes below — do not touch classes listed in other batch pages.

The original list was collected during a **partial** 3-GC-variant
(default/G1/ZGC) PostgreSQL run on Azure host `azureuser@20.80.105.49`, stopped
early once a dominant blocker was identified. Every FAIL on it carried the
runner's `NO-DB: connection-refused` signature, i.e. the session never opened —
the SASL/SCRAM bug (retired
`vertx-pg-sasl-scram-handshake-fails-20260812-FIXED` write-up), with the JNA
`Native.<clinit>` NPE in front of it.

## Result

Each class run one-per-process, `--Xmx 1500m`, `-Dcraton.batch=1`,
`DOCKER_HOST=unix:///var/run/docker.sock`, Testcontainers `postgres:18.4`.

| class | HotSpot 25 | dev tip, default (ZGC) | G1 | Generational | after fix |
|---|---|---|---|---|---|
| `BatchFetchTest` | PASS | PASS | PASS | PASS | PASS |
| `BatchQueryOnConnectionTest` | PASS | PASS | PASS | PASS | PASS |
| `BatchingConnectionTest` | PASS | PASS | PASS | **HANG/FAIL 4 of 6** | PASS |
| `BeforeExecutionIdGeneratorTypeTest` | PASS | PASS | PASS | PASS | PASS |
| `BlockSequenceGeneratorTest` | PASS | PASS | PASS | PASS | PASS |
| `BlockTableGeneratorTest` | PASS | PASS | PASS | PASS | PASS |
| `CacheTest` | PASS | PASS | PASS | PASS | PASS |
| `CachedQueryResultsGenerateStatisticsTest` | PASS | PASS | PASS | PASS | PASS |
| `CachedQueryResultsTest` | PASS | PASS | PASS | PASS | PASS |
| `CascadeComplicatedTest` | PASS | PASS | PASS | PASS | PASS |
| `CascadeComplicatedToOnesEagerTest` | PASS | PASS | PASS | PASS | PASS |
| `CascadeTest` | PASS | PASS | PASS | PASS | PASS |

Test counts match HotSpot's exactly in every green cell (e.g.
`BatchingConnectionTest` found=62 ok=61 skipped=1 on both), so these are real
passes and not a discovery that quietly found nothing.

**The defect:** `BatchingConnectionTest` timed out under
`-XX:+UseGenerationalGC` because the young non-moving sweep discarded ~40 700
reclaim decisions per cycle over 2 352 bytes of dead, all-zero-header
`new Object()`s. Fixed in `gc/src/gen_heap.rs`
(`zero_run_is_empty_object_run`); the write-up is the retired
`young-sweep-empty-object-run-unwind-20260812-FIXED` record. It is a general GC
defect — nothing about Hibernate Reactive, batching, or Postgres — that any
allocation-heavy workload can hit when the generational collector is selected
and the JIT has compiled anything.

**Still open, from the same investigation:**
[young-walk-zero-run-sibling-sites.md](young-walk-zero-run-sibling-sites.md) —
seven more walk sites in `gc/src/gen_heap.rs` misread the same benign shape.
Throughput and over-retention, not a hang.

## Repro

```bash
cd apps/hibernate-suite-runner   # on azureuser@20.80.105.49, /data/cratonvm
export DOCKER_HOST=unix:///var/run/docker.sock
export TESTCONTAINERS_RYUK_DISABLED=true
<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1500m -XX:+UseGenerationalGC \
    @common.args -Dcraton.batch=1 CratonRunner <ClassName>
# swap -XX:+UseGenerationalGC for -XX:+UseG1GC / -XX:+UseZGC / nothing (= ZGC)
# HotSpot control: $JAVA_HOME/bin/java @common.args -Dcraton.batch=1 CratonRunner <ClassName>
```

Note for the other batches: **the no-flag default is ZGC**, so a run with no GC
flag does not exercise the generational collector at all. Batch 01's one real
defect was invisible in three of the four configurations.
