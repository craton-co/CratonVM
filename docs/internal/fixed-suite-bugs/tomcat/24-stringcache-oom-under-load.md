# `StringCache.toString()` OOMs under sustained load — `TestMethodPerformance`

**Status:** OPEN. Confirmed CratonVM-only regression — passes on HotSpot in
the same fixture, same `-Xmx2g` default heap.

## Symptom

`org.apache.tomcat.util.http.TestMethodPerformance.testGetMethodPerformance`:

```
java.lang.OutOfMemoryError: Java heap space (new_object class_id 6 fields 4)
	at org.apache.tomcat.util.buf.StringCache.toString(StringCache.java:285)
	at org.apache.tomcat.util.http.TestMethodPerformance.testGetMethodPerformance(TestMethodPerformance.java:42)
```

Reproduced twice, at different points in the loop (597s and 227s wall time
across two runs on different `dev` tips) — the exact iteration count varies,
but the OOM inside `StringCache.toString()` under repeated invocation is
consistent both times.

## Analysis

This is a tight, repeated-call performance/stress test (the class name says
so) that calls `StringCache.toString()` (or the code path it exercises) many
times in a loop with no explicit heap growth expected — HotSpot completes it
at the same `-Xmx2g` without incident. An `OutOfMemoryError` from a single
hot method under sustained repeated calls, at a heap size HotSpot handles
fine, points at either:
- a memory leak in `StringCache`'s caching structure (entries never evicted/
  reused, growing unbounded), or
- excessive per-call allocation in the `toString()` path that a real cache
  should be avoiding entirely (defeating the purpose of `StringCache`).

Not root-caused to the exact allocation site in this session — the natural
next step is a heap-growth profile of `StringCache` under a tight
`toString()` loop, isolated from the rest of the Tomcat test harness.

## Reproduction

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref> -TimeoutSec 1500 -Parallel 1 -RunName stringcache-repro -Exe <cratonvm.exe>
```
Single class `org.apache.tomcat.util.http.TestMethodPerformance`. Consider
adding `--Xlog gc*=info` (CratonVM's unified-logging GC flag) to a standalone
repro to see whether old-gen occupancy climbs monotonically before the OOM,
which would confirm a leak vs. a single-allocation spike.
