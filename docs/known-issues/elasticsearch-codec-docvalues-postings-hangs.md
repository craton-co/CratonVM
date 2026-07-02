# Elasticsearch codec/doc-values/postings hangs

Status: open

Date observed: 2026-07-02

## Summary

Several Elasticsearch codec, doc-values, and postings tests hang under
CratonVM until the suite runner kills the process at the requested 300-second
hang timeout. HotSpot passes most of the same classes.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 7 CratonVM `HANG` rows in this family.
- 6 are CratonVM-only: HotSpot passed the same classes.
- 1 overlaps a HotSpot baseline failure.

Representative row:

```text
index=1352
module=server
class=org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests
CratonVM=HANG, 300.066s
HotSpot=PASS, 54.417s
```

Affected classes:

```text
org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests
org.elasticsearch.index.codec.bloomfilter.ES87BloomFilterPostingsFormatTests
org.elasticsearch.index.codec.postings.ES812PostingsFormatTests
org.elasticsearch.index.codec.tsdb.DocValuesForUtilTests
org.elasticsearch.index.codec.tsdb.es819.ES819TSDBDocValuesFormatVariableSkipIntervalTests
org.elasticsearch.index.codec.tsdb.ES87TSDBDocValuesFormatTests
org.elasticsearch.index.codec.tsdb.ES87TSDBDocValuesFormatVariableSkipIntervalTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1352 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-codec-docvalues-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```

## No-JIT partial evidence

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes. The partial no-JIT run found 9 codec/doc-values/postings HANG rows:

- 6 were CratonVM-only versus HotSpot PASS.
- 3 overlapped HotSpot baseline failures.
- The same 300-second runner timeout was used.

Additional no-JIT classes in this family include
`ES819TSDBDocValuesFormatTests` and `ES95TSDBDocValuesFormatTests`; under
JIT-on those classes crashed, while no-JIT hung.

Representative no-JIT row:

```text
index=1341
module=server
class=org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests
CratonVM no-JIT=HANG, 300.050s
HotSpot=PASS, 54.417s
```

Evidence:

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs\server.org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests.out.log
```

## Root-cause investigation 2026-07-02 (branch `fix/es-codec-docvalues-postings-hang`)

This family bundles **at least two distinct root causes**, not one. Diagnosed
with `--stack-dump-on-timeout N` against a worktree build
(`C:\craton\CratonVM-escodechang`, binary `cratonvm-escodechang.exe`).

### Cause 1 (doc-values classes): `BigDecimal.setScale` — real bug, partially fixed

`DocValuesForUtilTests.testEncodeDecode` calls
`org.apache.lucene.tests.util.TestUtil.nextLong(random(), 0, PackedInts.maxValue(bpv))`
up to `iterations × NUMERIC_BLOCK_SIZE` times (up to ~8M calls for large
`bpv`/block sizes). Whenever the requested range exceeds `Integer.MAX_VALUE`
(roughly half of all calls, `bpv >= 32`), `TestUtil.nextLong` falls back to
`new BigDecimal(range).multiply(new BigDecimal(random.nextDouble())).toBigInteger()`.
`BigDecimal.toBigInteger()` calls `setScale(0, ROUND_DOWN)`.

**Found:** `native_bd_set_scale`/`native_bd_set_scale_rounding`
(`native-builtins/src/lib.rs`) read the BigDecimal's exact decimal string,
**parsed it as `f64`**, then reformatted with `format!("{:.prec$}", v, ...)`.
This is two bugs in one:

1. **Correctness** — `f64` has ~17 significant decimal digits; any unscaled
   value wider than that (routine for `BigDecimal(double)`, whose exact
   decimal expansion can run to dozens of digits) silently lost precision.
   `123456789012345678901234567890.5.setScale(0, HALF_UP)` returned the wrong
   answer.
2. **Performance** — Rust's `f64::from_str` falls into a slow arbitrary-
   precision path for long decimal strings. Measured **~580µs per call**
   (vs HotSpot's ~1.6µs) — roughly 360×, and this path runs up to ~4M times
   in `testEncodeDecode`'s worst case.

**Fix:** rewrote both natives to operate on the existing binary limb-based
`crate::bigint::BigInt` (the read/write boundary already used by
`negate`/`abs`/`signum`/`precision` — see
`docs/internal/gaps/biginteger-limb-rewrite-scope.md`), implementing exact
Java `RoundingMode` semantics (UP/DOWN/CEILING/FLOOR/HALF_UP/HALF_DOWN/
HALF_EVEN/UNNECESSARY) via `divmod`+`shl`+`test_bit`, never touching `f64`.
Verified byte-for-byte against HotSpot across 21 cases (all rounding modes,
negative values, a 31-significant-digit operand) and against the crate's
8 `bigint::tests::*` differential tests (`cargo test -p cratonvm-native-builtins
--lib bigint::`, all pass).

**Measured impact:** `setScale(0, DOWN)` on a ~56-digit operand: 580µs → 408µs
per call (~30% faster). Full `BigInteger.valueOf(MAX).multiply(...).toBigInteger()`
pipeline: 946µs → 774µs per call (~18% faster).

**Not sufficient alone.** `DocValuesForUtilTests` still does not complete
within 290s after this fix (confirmed via a full untimed rerun). A further
microbenchmark (`new BigInteger("123456789012345678901234567890")` in a
tight loop, no arithmetic) measured **~400-770µs per plain BigInteger
construction** — i.e. the *allocation and native-dispatch* cost alone, not
any decimal-string arithmetic, dominates. This is the same class of gap
`docs/internal/gaps/biginteger-limb-rewrite-scope.md` already scopes for
BigInteger's own hot ops (mul/div/mod/modPow, already migrated to
`BigInt` in dev commits `eb0d06b2`/`607393d4`) — `BigDecimal`'s remaining
natives (`add`/`subtract`/`multiply`/`divide`/`intValue`/`longValue`/
`doubleValue`, still decimal-string-based) and the general per-object
native-allocation path are the likely next-highest-value targets, but a
full close of the ~300-500× gap is out of scope for a single fix.

### Cause 2 (postings classes): general interpreter throughput, NOT a deadlock

`ES85BloomFilterPostingsFormatTests` hangs in an entirely different place:
`BasePostingsFormatTestCase.testDocIDRunEnd` → `IndexWriter.updateDocuments`
→ `DocumentsWriterPerThread`/`IndexingChain`/`DocumentsWriterFlushControl`
→ (occasionally) `ConcurrentApproximatePriorityQueue.add` →
`ReentrantLock.tryLock()/unlock()`.

**Confirmed NOT a deadlock/livelock**: ~9500 watchdog stack-dump samples
over the run show the thread's call site continuously advancing through
normal indexing/flush code (`invertTerm` → `addTerm` → `finishDocument` →
`doAfterDocument` → `ramBytesUsed` → back into `updateDocuments` for the
next document, etc. — never stuck at one PC). The lock frames
(`ConcurrentApproximatePriorityQueue.add`/`ReentrantLock`) are transient,
not stuck. This class is simply CPU-bound interpreted-execution work
(document indexing, term inversion, flush bookkeeping) that HotSpot's JIT
finishes in ~54s and CratonVM's current interpreter/allocation throughput
cannot finish within 300s. `RandomPostingsTester.testTerms` (used by
`BasePostingsFormatTestCase.testRandom`, inherited by all three postings
classes in this family) also spawns 2+ raw `Thread`s per invocation and
`.join()`s them — investigated as a possible deadlock source, but CratonVM's
`thread_start`/`thread_join` completion path (`vm/src/vm/vm_exec.rs`) already
handles both normal-return and uncaught-exception thread termination
through the same `mark_dead` + termination-monitor-notify path, so this is
not implicated here.

**Status:** still open. Cause 1 has a landed, verified, low-risk partial fix
(the `setScale` correctness bug is real and fixed regardless of whether it
alone resolves the timeout). Cause 2 needs no code fix identified yet — it
needs the same broader interpreter/allocation throughput work as Cause 1's
residual. Full resolution of all 7 classes requires that broader effort, not
a single native-function patch.
