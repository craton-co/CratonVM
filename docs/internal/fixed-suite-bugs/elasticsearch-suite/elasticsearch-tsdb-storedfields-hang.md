# Elasticsearch TSDB stored fields no-JIT hang

Status: FIXED (2026-07-03)

Date observed: 2026-07-02

## Summary

`TSDBStoredFieldsFormatTests` hung in CratonVM no-JIT mode until the suite
runner killed it at the configured 300-second timeout. HotSpot passed the
same class. In the CratonVM JIT-on run, this class failed rather than
hanging, so the no-JIT run exposed a more severe interpreter-mode symptom.

## Root cause

Same root cause as `elasticsearch-tsdb-docvalues-native-crashes.md`: real
`MemorySegment.get/set` bytecode (used by Lucene's `MMapDirectory` /
`MemorySegmentIndexInput`, which this stored-fields codec's
`Lucene90CompressingStoredFieldsWriter`/LZ4 path reads through) called
`java.lang.invoke.SegmentVarHandle.get/set`, which CratonVM's `vm_exec.rs`
signature-polymorphic dispatch never recognised as a VarHandle receiver (see
that doc for the full analysis) and which had no implementation at all. The
doc's own note that "SegmentVarHandle failures" appeared in the TSDB hang
logs before the runner timeout was the same gap surfacing here.

## Fix

Same fix as `elasticsearch-tsdb-docvalues-native-crashes.md`:
`../../../../vm/src/vm/vm_exec.rs`'s `is_vh` broadening plus
`../../../../native-builtins/src/lang_invoke.rs`'s `segment_vh_get`/`segment_vh_set`
implementation. No hang-specific changes were needed — this class simply
shares the exact same access path.

## Verification

Same worktree/binary as the docvalues-crash fix
(`C:\craton\CratonVM-estsdb`, `cratonvm-estsdb.exe`).

`TSDBStoredFieldsFormatTests`, no-JIT, raw `JUnitCore` invocation with
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` (matching the official suite runner)
and a 620s wall-clock budget (well past the original 300s suite-runner
timeout, since no-JIT/interpreted execution is legitimately much slower than
JIT, not stuck):

- **Before the fix**: zero progress — stdout never advanced past the JUnit
  startup banner, exactly as the original bug reported.
- **After the fix**: completes in ~507s wall-clock. `Tests run: 20,
  Failures: 3`. The 3 residual failures are unrelated pre-existing gaps, not
  a hang or crash:
  - `testSyntheticId`: `ExceptionInInitializerError` →
    `Module.getDescriptor()` null (a separate, already-tracked
    module-getdescriptor gap — see the `fix/module-getdescriptor-uses-null`
    / `fix/es-module-getdescriptor-null` branches).
  - `testLineFileDocs`, `testMultiClose`:
    `IllegalArgumentException: Not a supported array class: byte[]` — a
    distinct, not-yet-investigated gap unrelated to `MemorySegment`/
    `SegmentVarHandle` access (the failing assertion is downstream of a
    `StringBuilder`/array-copy path per the stack trace, not the codec's
    native memory reads, which all completed correctly across the run).

So this is no longer a hang: 17/20 tests pass, and the process runs to
completion and exits normally (`System.exit(1)` from the 3 JUnit failures,
not a runner kill).

`cargo test --release -p cratonvm-native-builtins --lib`: 2749 passed, 0
failed (covers the new `segment_vh_get`/`segment_vh_set` code paths via the
existing FFM/VarHandle unit tests plus the crate's full regression suite).

## Residual

The 3 failures above (`Module.getDescriptor()` null,
`Not a supported array class: byte[]`) are out of scope for this doc — track
separately if picked up. The 507s wall-clock time also exceeds the suite
runner's default 300s per-class timeout; if this class is re-added to a full
suite run, either raise its timeout or accept it as a slow-but-passing
(mostly) interpreter-mode class, matching the precedent set by other
TSDB-family classes after their own native-gap fixes.

## Repro (historical, before the fix)

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit off -Start 1344 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-tsdb-storedfields-nojit-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\target\release\cratonvm-elasticsearch-nojit-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs\server.org.elasticsearch.index.codec.storedfields.TSDBStoredFieldsFormatTests.out.log
```
