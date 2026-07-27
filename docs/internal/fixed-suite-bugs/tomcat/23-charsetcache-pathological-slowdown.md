# `TestCharsetCachePerformance` — the cache is 3x SLOWER than no cache at all

**Status:** OPEN, real algorithmic/performance bug (not just "CratonVM is
generally slower than HotSpot" — the cache is pathologically slower than
the deliberately-uncached baseline in the SAME test, on the SAME VM).
Confirmed CratonVM-only — passes on HotSpot in the same fixture.

## Symptom

`org.apache.tomcat.util.buf.TestCharsetCachePerformance` prints its own
timing results before hitting the per-class timeout:

```
org.apache.tomcat.util.buf.TestCharsetCachePerformance$NoCsCache: 212099534100ns   (~212s)
org.apache.tomcat.util.buf.TestCharsetCachePerformance$FullCsCache: 685391186700ns  (~685s, still running when the run was killed by a later timeout)
```

The test's whole premise is that the cached path (`FullCsCache`) should be
**faster** than the uncached baseline (`NoCsCache`) — that's what the test
asserts. On CratonVM it's **~3.2x slower**, not faster, and slow enough on
its own (11+ minutes for what should be a fast cached-lookup microbenchmark)
to blow through even a 1500s per-class timeout when combined with the rest
of the class.

## Analysis

This isn't the general "interpreter/JIT throughput ceiling" documented in
`04-embedded-server-throughput-wall-OPEN.md` (that's roughly-uniform
overhead vs. HotSpot across the board) — a 3x slowdown *of the supposedly-
optimized path relative to the deliberately-unoptimized one in the same
process* points at a specific data-structure or locking defect in whatever
CratonVM-side charset-cache implementation backs
`org.apache.tomcat.util.buf.CharsetCache` (or wherever the "full cache"
lookup is implemented) — e.g. a linear scan where a hash lookup is expected,
lock contention on every lookup, or a cache that's being invalidated/rebuilt
on every call instead of reused. Not root-caused to a specific file/line in
this session.

## Reproduction

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref> -TimeoutSec 1500 -Parallel 1 -RunName charsetcache-repro -Exe <cratonvm.exe>
```
Single class, single worker — the printed `NoCsCache`/`FullCsCache`
nanosecond timings appear directly in
`.suite\results\<run>\real-jit\org.apache.tomcat.util.buf.TestCharsetCachePerformance.log`
even if the class as a whole times out before finishing all assertions.
