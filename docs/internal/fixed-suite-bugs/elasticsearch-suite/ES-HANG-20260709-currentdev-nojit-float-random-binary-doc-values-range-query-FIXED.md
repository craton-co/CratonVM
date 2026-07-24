# ES HANG - FloatRandomBinaryDocValuesRangeQueryTests

Status: FIXED and retired from `../../../known-issues` on 2026-07-13.

Historical observations:

- Current dev JIT crashed with rc=139.
- Current dev `--nojit` timed out at 600 seconds.
- An earlier rerun reported `IllegalMonitorStateException` while releasing a read lock.

Root causes closed:

1. The existing RRWL / young-GC fixes had eliminated the historical lock-state hang, but the class still failed during randomized-test teardown.
2. CratonVM's native `Formatter.format(String,Object[])` replaced a caller-provided `StringBuilder` Appendable with a `String`. Formatting output was lost and `Formatter.flush()` later tried to dispatch `String.flush()V`.
3. The real `ThreadPoolExecutor.shutdown()` bridge only changed `ctl` to SHUTDOWN. It did not wake idle workers blocked in `LinkedBlockingQueue.take()`, so the Lucene suite's two `LuceneTestCase` executor threads leaked.

Fix:

- Preserve caller-owned Appendables in the native Formatter path and append to them instead of replacing them.
- Avoid a flush dispatch for the in-memory non-Flushable builder sink.
- Wake real executor workers after publishing SHUTDOWN so they observe the state transition and terminate.

Validation on Azure host `20.83.144.174`:

- Worktree: `/data/victor-worktrees/cratonvm-es-floatrange-residuals-20260712`
- Binary: `/data/data/cratonvm-targets/es-floatrange-residuals-20260712/cratonvm-es-floatrange-residuals-20260712.bin`
- Elasticsearch fixture: `/data/data/es-jit-deopt-gc-bundle-20260708-214648/elasticsearch`
- Seed: `B17AC9D3E1F2A0C4`
- Focused `Formatter(Appendable)` probe: PASS; supplied builder retained `directok` and emitted no bogus flush lookup.
- HotSpot control: PASS, 6 tests, 6.372s.
- CratonVM `--nojit`: PASS, 6 tests, 37.577s.
- CratonVM JIT: PASS, 6 tests, 38.044s.

The exact Float class no longer hangs, crashes, throws the historical read-lock error, or leaks its suite executor workers.
