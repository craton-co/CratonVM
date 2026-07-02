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
Watchdog-sample histograms (thousands of dumps per run) were used as a poor
man's profiler throughout; frames parked at `pc=0` = threads caught at frame
entry = interpreter call overhead dominating.

### Registration discovery that reframed the whole fix

The full `register_biginteger_natives`/`register_bigdecimal_natives` blocks
(including `pow`, `setScale`, and every `<init>`) live inside
`register_synthetic_overrides`, which **only runs under `--synthetic-jdk`**.
In the default real-JDK mode, only the lean
`register_biginteger_arithmetic_overrides`/`register_bigdecimal_arithmetic_overrides`
sets are dispatched. So in real mode `BigInteger.pow`, `BigDecimal.setScale`,
and all BigDecimal constructors were running **real JDK bytecode,
interpreted** — and constructors are JIT-banned wholesale (`vm/src/jit/
skip_list.rs`, the A1.4 `<init>` ban), so they can never warm up. Any fix
placed in the synthetic-only block is inert in every suite run.

### Cause 1 (doc-values classes): interpreted `java.math` hot path — FIXED

`DocValuesForUtilTests.testEncodeDecode` calls
`TestUtil.nextLong(random(), 0, PackedInts.maxValue(bpv))` up to
`iterations × NUMERIC_BLOCK_SIZE` times (up to ~8M). For ranges >
`Integer.MAX_VALUE` (~half the calls), `TestUtil.nextLong` runs
`new BigDecimal(range).multiply(new BigDecimal(random.nextDouble())).toBigInteger()`.
On CratonVM that pipeline cost **~945µs per call** (HotSpot: ~1.6µs), from:

- `new BigDecimal(double)` (interpreted ctor) → `BigInteger.pow` computing
  `5^(-exponent)` (~52 squarings per random double) — pow itself interpreted.
- `toBigInteger()` → `setScale(0, DOWN)` → interpreted `divideAndRound` →
  `MutableBigInteger` long division.
- `BigInteger.valueOf(J)` → interpreted `new BigInteger(long)` 3-4× per call
  (the `valueOf`→`<init>(J)`→`Number.<init>` frame chain alone was ~26% of
  watchdog samples).
- The *active* lean natives `compareTo`/`equals`/`negate`/`toString` still
  paid an O(words²) mag→decimal-string conversion per call, and
  `setScale`/`intValue`/`longValue` (synthetic-block versions) round-tripped
  through `f64` — silently wrong beyond ~17 significant digits on top of slow.

**Fix (all in `native-builtins/src/lib.rs`, registered in the real-JDK lean
override sets):**

- New exact natives: `BigInteger.pow` (limb square-and-multiply),
  `BigDecimal.setScale(I)`/`(II)` (limb divmod + exact RoundingMode
  semantics incl. UNNECESSARY-throws), `BigDecimal.toBigInteger`,
  `BigInteger.valueOf(J)`, `BigDecimal.<init>(D)` (exact binary expansion via
  sign/exponent/significand decomposition — `0.1` produces the 55-digit
  form), `BigDecimal.<init>(BigInteger)` (compactValFor split, stores the
  caller's BigInteger only when inflated). `<init>` natives are dispatched
  in real-JDK mode — `java.util.Random(J)`'s seeded LCG ctor already
  depends on that.
- Migrated to the limb `BigInt` path: `compareTo`/`equals`/`negate`/
  `toString` (BigInteger), `intValue`/`longValue` (BigDecimal — now correct
  two's-complement narrowing), `bd_alloc_bigint`/`bd_write_into_bigint`
  (direct `intCompact`/`intVal` field writes, JDK-lazy `precision=0`, null
  `intVal` on the compact path exactly like `BigDecimal.valueOf(long,int)`).

**Verified:** three Java differential batteries run byte-identical to
HotSpot 25 — `SetScaleCorrect` (21 cases, every rounding mode, 31-digit
operand), `MathCorrect` (pow edge cases incl. `0^0`/negative-exponent
message, toBigInteger/longValue narrowing, a 50-round seeded
`TestUtil.nextLong`-dance checksum, `new BigDecimal(0.1)`'s exact 55-digit
expansion), `BdCanary` (serialization round-trip of a natively-built
compact BigDecimal, HashMap/TreeMap equals-vs-compareTo semantics, lazy
`precision()`, `valueOf` value semantics). The crate's 8
`bigint::tests::*` differential tests pass.

**Measured (2000-iteration microbenches, same machine, same run):**
`5.pow(53)` 228ms → **8ms**; `setScale(0,DOWN)` on ~50-digit operands
1161ms → **7ms**; the full nextLong pipeline 1891ms → **38ms** (~945µs →
~15µs per call, stable at 20k iterations — no JIT warmup dependence).

**Residual for this class:** after the fix, watchdog samples show the
math work is gone from the profile entirely; **100% of deep leaves are the
`random()` chain** — `RandomizedContext.context` →
`WeakHashMap.getTable`/`getPerThread`. `CRATONVM_DBG_JITC=1` shows why:
`RandomizedContext.context` (a `static synchronized` method) hits a
permanent `compile-bail backend_attempted=true`, and
`WeakHashMap.getTable`/`expungeStaleEntries` `bg-compile` at C1 but never
publish code (no `full-compile` line, silently). So the per-iteration
`random()` calls run interpreted forever. That is carrotsearch/Lucene
framework code — not shadowable with natives under the project's
JDK-intrinsics-only rule; the fix belongs to the JIT workstream
(synchronized-method compilation + the silent bg-compile no-publish).

End-to-end after the fix (seed `B17AC9D3E1F2A0C4`): the class no longer
hard-hangs — it runs to randomizedtesting's own 580s suite timeout
(`-Dtests.timeoutSuite=580000` from the runner), `testEncodeDecode` is
abandoned there while `testEncodeDecodeBitsPerValue` PASSES, and the
process exits cleanly at 603s total (was: killed by the runner at 300s
with no test completing). The remaining gap to the 300s envelope is the
interpreted `random()` chain above.

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
next document, etc. — never stuck at one PC). The lock frames are
transient, not stuck. This class is CPU-bound interpreted-execution work
(document indexing, term inversion, flush bookkeeping) that HotSpot's JIT
finishes in ~54s. `RandomPostingsTester.testTerms` (used by
`testRandom` in all three postings classes) spawns raw `Thread`s and
`.join()`s them — investigated as a deadlock candidate and ruled out
(`thread_start`/`thread_join` in `vm/src/vm/vm_exec.rs` handle both
normal-return and uncaught-exception termination through the same
`mark_dead` + termination-monitor-notify path).

**Status:** still open as a *suite result* (the classes don't pass within
300s yet), but the `java.math` layer is fixed, verified, and a large
across-the-board win for anything BigDecimal/BigInteger-heavy. The
remaining work items are both JIT-workstream items, not math natives:
(a) `static synchronized` methods permanently bail the JIT backend;
(b) some C1 bg-compiles never publish (`WeakHashMap.getTable`);
(c) the indexing-pipeline throughput gap behind the postings classes.
