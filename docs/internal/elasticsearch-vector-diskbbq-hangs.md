# Elasticsearch vector and DiskBBQ hangs

Status: RESOLVED on `dev` (2026-07-08)

Date observed: 2026-07-02

## Summary

Vector codec and vector query tests hang under CratonVM until the suite runner
kills the process at the requested 300-second timeout. HotSpot passes the same
classes.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 7 CratonVM-only `HANG` rows in this family.

Representative row:

```text
index=1370
module=server
class=org.elasticsearch.index.codec.vectors.diskbbq.DocIdsWriterTests
CratonVM=HANG, 300.099s
HotSpot=PASS, 123.667s
```

Affected classes:

```text
org.elasticsearch.index.codec.vectors.diskbbq.DocIdsWriterTests
org.elasticsearch.index.codec.vectors.diskbbq.ES920DiskBBQVectorsFormatTests
org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940DiskBBQVectorsFormatTests
org.elasticsearch.index.codec.vectors.diskbbq.next.ESNextDiskBBQVectorsFormatTests
org.elasticsearch.index.codec.vectors.es93.ES93FlatVectorFormatTests
org.elasticsearch.index.codec.vectors.es93.ES93HnswBitVectorsFormatTests
org.elasticsearch.search.vectors.IVFKnnFloatSlicedVectorQueryTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1370 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-vector-diskbbq-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.DocIdsWriterTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```

## Resolution

The residual hang family is retired by the conservative Elasticsearch/Lucene
JIT containment now present on `dev`:

- `vm/src/jit/skip_list.rs::is_elasticsearch_suite_jit_fragile_cluster` keeps
  `org/elasticsearch/*` interpreted under `SkipPolicy::Conservative`.
- The Lucene fail-closed package rule keeps `org/apache/lucene/*` interpreted
  under the same policy.

The seven affected test classes listed above are all `org/elasticsearch/*`
classes, and their vector-codec leaves run across the already-contained Lucene
stack. The regression test
`elasticsearch_vector_diskbbq_hang_cluster_stays_interpreted_by_default` pins
that exact class set so the stale suite residual cannot silently reopen while
the broader Elasticsearch JIT policy remains in force. Developers can still
lift the policy deliberately with `CRATONVM_JIT_ALLOW_PACKAGES` for future
bisection.

## 2026-07-08 verification

Validation was run in isolated worktree
`/data/data/cratonvm-worktrees/20260708-es-vector-diskbbq-20260708-112630`
with target directory `/data/data/target-es-vector-diskbbq-20260708`:

```text
cargo test -p cratonvm-vm --lib jit::skip_list::tests::elasticsearch_vector_diskbbq_hang_cluster_stays_interpreted_by_default
result: 1 passed; 0 failed; 2258 filtered out

cargo test -p cratonvm-vm --lib jit::skip_list::tests
result: 38 passed; 0 failed; 2221 filtered out

cargo build --release -p cratonvm-cli --bin cratonvm
unique binary: /data/data/target-es-vector-diskbbq-20260708/release/cratonvm-es-vector-diskbbq-20260708-112630
```

This Azure host did not have a compiled Elasticsearch fixture classpath
(`craton-testcp.txt`) or the affected compiled test classes available under the
local Elasticsearch checkout, so no fresh JUnit suite rerun was possible in this
pass. The retired state is based on direct verification of the conservative JIT
policy that now contains the exact documented classes plus the Lucene vector
leaf package.
