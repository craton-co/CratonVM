# Elasticsearch postings-format timeout from RamUsageTester undercharge

Status: fixed 2026-07-08.

The surviving postings-format residual was not a deadlock and not a new Lucene
postings corruption. The actionable root cause was the CratonVM native bridge
for `org.apache.lucene.tests.util.RamUsageTester.ramUsed(Object)`: it returned a
flat 1024 bytes for every `org.apache.lucene.document.Document`.

`BasePostingsFormatTestCase.testDocIDRunEnd` uses `LineFileDocs.nextDoc()`, keeps
only the `body` field, indexes documents until `bytesIndexed >= atLeast(100) *
1024`, and increments `bytesIndexed` with `RamUsageTester.ramUsed(justBodyDoc)`.
HotSpot/Lucene 10.4 reports roughly `align8(376 + 2 * body.length())` for these
body-only documents, averaging about 2.2 KiB for seed `B17AC9D3E1F2A0C4`.
CratonVM therefore indexed roughly twice as many documents as the test intended,
which turned an already interpreter-heavy postings path into a class-level
suite timeout.

## Fix

`../../../../native-builtins/src/lib.rs` now inspects Lucene `Document.fields`, reads each
Lucene `Field.fieldsData` string payload, and estimates the document charge from
actual string length. Unknown shapes fall back to the old 1024-byte conservative
charge. The helper pins the field list across re-entrant `List.size()` / `get()`
upcalls so it remains correct under moving GC.

The test mock gained Lucene field-name mappings solely for focused native unit
tests; production uses normal class metadata field lookup.

## Validation

Focused Rust test:

```text
cargo test -p cratonvm-native-builtins lucene_ram_usage_tests --lib
result: 3 passed
```

Fresh unique binaries on the Azure host:

```text
baseline: /data/data/bin/cratonvm-espostings-residuals-142548-baseline
fixed:    /data/data/bin/cratonvm-espostings-residuals-142548-fixed
```

Synthetic one-field document probe, `DocumentRamUsageProbe 160 1`:

```text
HotSpot:          1912..1920 bytes
CratonVM before: 1024 bytes for every document
CratonVM after:  1896..1904 bytes
```

Exact `LineFileDocs` body-only loop used by `BasePostingsFormatTestCase`, target
102400 bytes, seed `B17AC9D3E1F2A0C4`:

```text
HotSpot:          count=44 bytes=102664 max=3488
CratonVM before: count=100 bytes=102400 max=1024
CratonVM after:  count=43 bytes=107848 max=6000
```

The Azure host did not have a compiled Elasticsearch fixture, so the full
`ES85BloomFilterPostingsFormatTests` class was not rerun there. The validation
above exercises the exact byte-budget gate that controlled the residual
class-level timeout. If a future full-suite run still times out, it should be
tracked as a new residual after this RamUsageTester undercharge fix.


## Historical Notes

## Summary

`ES85BloomFilterPostingsFormatTests`, `ES87BloomFilterPostingsFormatTests`,
and `ES812PostingsFormatTests` hang under CratonVM until the suite runner's
300-second timeout kills the process. HotSpot passes the same classes in
~54s.

This is the surviving half of a two-cause bug originally filed together as
"codec/doc-values/postings hangs". The other half (doc-values classes —
`DocValuesForUtilTests`, `ES87TSDBDocValuesFormatTests`, and friends) was
root-caused to an interpreted `java.math` hot path and **fixed** (merged to
dev as commit `794266fc`). The full investigation history for both causes,
including the fixed doc-values cause, lives in
`docs/internal/elasticsearch-codec-docvalues-postings-hangs.md` (moved
there since it's now historical for that half). The doc-values classes'
*residual* slowness (an unrelated, still-open `RandomizedContext`/
`WeakHashMap` race condition) is tracked separately in
`docs/known-issues/elasticsearch-randomizedcontext-per-thread-null.md` —
not here.

## Root cause: general interpreter throughput, not a deadlock

`ES85BloomFilterPostingsFormatTests` hangs in
`BasePostingsFormatTestCase.testDocIDRunEnd` →
`IndexWriter.updateDocuments` →
`DocumentsWriterPerThread`/`IndexingChain`/`DocumentsWriterFlushControl` →
(occasionally) `ConcurrentApproximatePriorityQueue.add` →
`ReentrantLock.tryLock()/unlock()`.

**Confirmed NOT a deadlock/livelock**: ~9500 watchdog stack-dump samples
over a run show the thread's call site continuously advancing through
normal indexing/flush code (`invertTerm` → `addTerm` → `finishDocument` →
`doAfterDocument` → `ramBytesUsed` → back into `updateDocuments` for the
next document, etc. — never stuck at one PC). The lock frames are
transient, not stuck. This is CPU-bound interpreted-execution work
(document indexing, term inversion, flush bookkeeping) that HotSpot's JIT
finishes in ~54s. `RandomPostingsTester.testTerms` (used by `testRandom` in
all three postings classes) spawns raw `Thread`s and `.join()`s them —
investigated as a deadlock candidate and ruled out (`thread_start`/
`thread_join` in `../../../../vm/src/vm/vm_exec.rs` handle both normal-return and
uncaught-exception termination through the same `mark_dead` +
termination-monitor-notify path).

Closing this gap is an open-ended JIT/interpreter performance project
(see `../../../feature-designs/wire-tiered-manager.md`), not a single scoped
bug — HotSpot's tiered JIT is simply faster on this workload today.

## 2026-07-04 re-check: a real, distinct tiered-manager bug was found and fixed along the way, but it does NOT close this gap

While re-investigating this doc's earlier speculation about *why* the
doc-values residual stayed slow (a theory involving `ACC_SYNCHRONIZED`
methods permanently bailing JIT compilation, and C1 background-compiles
silently never publishing), the synchronized-method theory did not hold up
in current code, but the bg-compile-publish theory uncovered a real bug:
`../../../../jit/src/tiered.rs`'s `CompilerCore::complete_task` was marking a method's
tier as reached even when the background compile attempt failed and
published nothing, permanently starving it of ever running compiled code
with zero logging. Fixed on branch `fix/jit-tiered-bgcompile-failure-stall`
(merged to dev) — full writeup in
`docs/internal/elasticsearch-codec-docvalues-postings-hangs.md`'s
"Addendum 2026-07-03/04".

Re-ran `ES85BloomFilterPostingsFormatTests` directly against the fixed
binary: **still times out at ~300s**, identical shape (progresses past
`testHashTerms`, hangs later in the `testDocsAndFreqsAndPositions...`
family). Confirms the root cause above — general interpreted-execution
throughput — rather than methods getting stuck uncompiled. The
tiered-manager fix is real and generally beneficial (any hot method whose
first bg-compile attempt fails for a transient reason no longer gets stuck
interpreting forever), but it is not what's gating this suite result.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1352 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-codec-postings-hang-repro `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir <workdir> `
  -Exe <cratonvm.exe>
```

Direct invocation (index-independent — see the sibling
`elasticsearch-randomizedcontext-per-thread-null.md` doc's Repro section
for the full JUnitCore flag set to adapt, swapping in
`org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests`
as the target class).

## Affected classes

```text
org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests
org.elasticsearch.index.codec.bloomfilter.ES87BloomFilterPostingsFormatTests
org.elasticsearch.index.codec.postings.ES812PostingsFormatTests
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-escodec2\es85-postings-fix.out.log (2026-07-04 re-check against the tiered-manager-fixed binary — TIMEOUT after ~300.8s)
```
