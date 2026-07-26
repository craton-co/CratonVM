# Elasticsearch codec/doc-values/postings hangs

Status: ARCHIVED 2026-07-03 (this file is the historical investigation
record). Live status: cause 1 (doc-values `java.math` hot path) is FIXED —
see `docs/known-issues/elasticsearch-randomizedcontext-per-thread-null.md`
for its unrelated residual. Cause 2 (postings classes) was fixed on
2026-07-08 by correcting the CratonVM `RamUsageTester.ramUsed(Document)`
shortcut; see
`../fixed-suite-bugs/elasticsearch-postings-ramusage-undercharge-FIXED.md`.

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

**Fix (all in `../../../../native-builtins/src/lib.rs`, registered in the real-JDK lean
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
(`thread_start`/`thread_join` in `../../../../vm/src/vm/vm_exec.rs` handle both
normal-return and uncaught-exception termination through the same
`mark_dead` + termination-monitor-notify path).

**Status:** still open as a *suite result* (the classes don't pass within
300s yet), but the `java.math` layer is fixed, verified, and a large
across-the-board win for anything BigDecimal/BigInteger-heavy. The
remaining work items are both JIT-workstream items, not math natives:
(a) `static synchronized` methods permanently bail the JIT backend;
(b) some C1 bg-compiles never publish (`WeakHashMap.getTable`);
(c) the indexing-pipeline throughput gap behind the postings classes.

## Addendum 2026-07-03/04: (a) and (b) re-investigated, one real bug found and fixed (unrelated to synchronized methods); postings hang (c) confirmed still open

A follow-up session re-checked items (a) and (b) above against current dev
(post-794266fc) before attempting further fixes.

**Item (a), "static synchronized methods permanently bail the JIT
backend", does not hold up as stated.** Read through the full compile
pipeline (`jit/src/lib.rs::try_compile`, `jit/src/x64.rs::compile*`) and the
invoke-dispatch paths (`../../../../vm/src/runtime/interpreter.rs`
`execute_invokestatic_cached`/`execute_invokevirtual_cached`/
`try_jit_upgrade_with_gate`): there is no `is_synchronized`/`ACC_SYNCHRONIZED`
check anywhere in the actual codegen backend, and the static-invoke path has
no synchronized-exclusion at all (only the virtual/instance path excludes
`cached.is_synchronized` from its monomorphic JIT fast path, for an
unrelated reason — the JIT body doesn't itself acquire the method monitor,
so that specific fast path is skipped and the interpreter's monitor-wrapping
call path is used instead; this does not block compilation). If
`RandomizedContext.context` really showed a permanent compile-bail, the
likely explanation is something incidental in its bytecode shape (an
exception table, a resolver miss, etc.) hitting an unrelated gate — not
`ACC_SYNCHRONIZED` itself. This is also consistent with, and superseded by,
the much more thorough 2026-07-03 investigation in
`docs/known-issues/elasticsearch-randomizedcontext-per-thread-null.md`,
which bisected the *actual* `RandomizedContext`/`WeakHashMap` residual to a
JIT-volume/timing race condition, explicitly ruling out "one miscompiled
(or uncompiled) method" as the explanation. Chasing (a) further is a dead
end per that doc; see it instead for the real state of this residual
(GC bug found+fixed, race condition still open).

**Item (b), "C1 bg-compiles for `WeakHashMap.getTable` never publish,
silently", DID uncover a real, distinct, previously-unknown bug** — not
specific to `WeakHashMap`, and not the cause of (a)'s symptom, but a
genuine defect in the tiered-compilation manager's bookkeeping:
`../../../../jit/src/tiered.rs`'s `CompilerCore::complete_task` unconditionally set
`state.current_tier = tier` and counted a `c1_compilations`/`c2_compilations`
stat whenever the background worker finished a compile *attempt* —
regardless of whether `compile_fn` (`background_compile_task` in
`interpreter.rs`, via `try_jit_compile_callee`/`compile_osr_artifact`)
actually produced and published a body into `shared.jit_cache`. The VM side
discarded the `Option` result with `let _ = ...`. Once `current_tier` reads
"already at C1", `should_compile`'s `Interpreter`/`C1` arms never
recommend that method again — so a method whose *first* bg-compile attempt
failed for any non-bail-listed reason (transient code-cache-cap, an
in-flight class redefine, or any of the dozen resolver/native-shadow gates
in `try_jit_upgrade_with_gate` that aren't in the permanent bail-list) got
silently and permanently stuck interpreting, forever, with zero logging.
The existing doc comment on `background_compile_task` even asserted the
intended (but unimplemented) behavior verbatim: "a later mutator invocation
re-attempts" — which was false before this fix.

**Fix** (branch `fix/jit-tiered-bgcompile-failure-stall`, worktree
`C:\craton\CratonVM-escodec2`): `CompileFn` now returns
`(elapsed_ms, success)` instead of bare `elapsed_ms`; `complete_task` takes
an explicit `success: bool` and only advances `current_tier` / counts the
tier stat on success, incrementing a new `MethodState::tier_fail_count` on
failure; `should_compile` gives up on a method after
`MAX_TIER_FAIL_RETRIES = 3` consecutive failures (mirroring the existing
`c2_bailout` "3+ deopts" convention) so a permanently-failing hot method
doesn't get re-enqueued on every single invocation forever.
`background_compile_task`'s two compile call sites (OSR and normal) now
report `.is_some()` as the real success signal. 3 new regression tests
added (`failed_compile_does_not_advance_tier_or_stats`,
`failed_compile_is_retried_up_to_the_fail_limit`,
`successful_compile_after_a_failure_resets_the_fail_count`); all 51
`jit::tiered::` tests plus the rest of the `cratonvm-jit --lib` suite pass
(866 passed; the only 4 failures are pre-existing `aarch64::tests::*`
release-profile artifacts — those tests assert on `debug_assert!` firing,
which is compiled out under `--release` regardless of this change, and
`aarch64.rs` is untouched by this fix).

**Verified this fix does NOT close the postings hang.** Ran
`ES85BloomFilterPostingsFormatTests` directly against the fixed binary —
it still times out at ~300s with the identical shape (progresses past
`testHashTerms`, hangs later in the `testDocsAndFreqsAndPositions...`
family). This is expected and consistent with the "general interpreter
throughput, not a discrete bug" diagnosis in Cause 2 above — the
tiered-manager bug fixed here prevents *some* hot methods from getting
permanently stuck uncompiled, which is a real, generally-beneficial
correctness fix, but it is not the (or at least not the whole) mechanism
behind the postings indexing-pipeline being CPU-bound-slow relative to
HotSpot's JIT. Closing that gap remains an open-ended JIT/interpreter
performance project (see `../../../feature-designs/wire-tiered-manager.md`),
not a scoped bug.
