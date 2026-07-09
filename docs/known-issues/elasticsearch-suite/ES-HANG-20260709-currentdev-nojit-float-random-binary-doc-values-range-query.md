# ES HANG - current-dev --nojit FloatRandomBinaryDocValuesRangeQueryTests

Status: OPEN

Class:
- `server org.elasticsearch.lucene.queries.FloatRandomBinaryDocValuesRangeQueryTests`

Source:
- Probe run: `es-faildocs-probe-20260709-073704`
- Binary: `/data/data/cratonvm-targets/20260709-073704-es-fail-docs/release/cratonvm-20260709-073704-es-fail-docs`
- Hang timeout: 600 seconds.

Results:
- HotSpot: PASS, rc=0, 3.026s, 6 tests.
- CratonVM JIT: CRASH, rc=139, 4.223s.
- CratonVM --nojit: HANG, rc=TIMEOUT, 600.007s.

Old full-rerun signal:
- Run `es-nonpassed-rerun-20260708-191002` had 1 direct FAIL row for this class.
- Note: `java.lang.IllegalMonitorStateException: attempt to unlock read lock, not locked by current thread`.

Interpretation:
- The old Java-level read-lock ownership failure and the current `--nojit` hang likely point at a concurrency/lock-state residual in the same test area.
- Current JIT exits 139 before a Java-level exception is captured.

Next investigation:
- Add a focused ReentrantReadWriteLock/read-lock ownership probe around the Lucene query path.
- For the ES class, capture a thread dump near 590s under `--nojit` to see whether it is blocked on lock acquisition, randomizedtesting leak checks, or Lucene query iteration.
