# `ZipContentTests` HANG — disk-capacity theory checked and RULED OUT for this run; actual driver looks like GC/allocation pressure from its multi-gigabyte zip64 fixture, on a class that is already borderline-slow

**Status: RETIRED 2026-08-10 — the GC/allocation-pressure reading below is
REFUTED by measurement, and the class is root-caused elsewhere. Live page:
`known-issues/springboot/zipcontenttests-bytebuffer-accessor-call-cost-20260810.md`.**

What this page got right: the disk-capacity theory does not fit, and it says so
having actually checked rather than assumed. What it got wrong, and the numbers
that settle it:

* **GC/allocation pressure is not the driver.** `-Xmx 8g` runs the class in
  308.9s against `-Xmx 2g`'s 315.5s — a 2% difference. Quadrupling the heap
  changes nothing. (The same lever turned a 300s overrun into a 24.3s pass for
  `HttpComponentsClientHttpConnectorBuilderTests`, so it is a real lever that
  this class simply does not respond to.)
* **It is not a hang.** Run without the 300s ceiling it PASSES 29/29 in 315.5s
  (JIT) and 219.1s (`--nojit`), against HotSpot's 22.4s.
* **The 0-byte `.out.log` inference does not hold.** `SbRunner` prints nothing
  until `launcher.execute(req)` *returns* (`sb-runner/SbRunner.java:80,83`), so
  an empty stdout means "did not finish" and cannot locate the stall. The
  claim below that "it hangs during class initialization, before any test
  method runs" rests entirely on that inference and is unfounded — all 29 tests
  do run.
* **The actual cost** is `java.nio.ByteBuffer` scalar accessors, which the zip
  header reader calls once per field: `getShort`+`getInt` measure 2234 ns/op
  against HotSpot's 1.36. File I/O is *faster* than HotSpot here and zlib is
  within 2x.

Kept for the run history and the cross-run table, which are still accurate.

---

**Original status: OPEN — not root-caused; a throughput/GC-pressure problem on
an inherently heavy test class, not the previously-suspected disk-space
environmental artifact.**

## Why this doc exists: checking, not assuming, the known disk-capacity gotcha

This class has a known environmental sensitivity: one of its tests,
`openWhenZip64ThatExceedsZipSizeLimitOpensZip`
(`apps/spring-boot/loader/spring-boot-loader/src/test/java/org/springframework/boot/loader/zip/ZipContentTests.java:302-333`),
guards itself with
`Assumptions.assumeTrue(this.tempDir.getFreeSpace() > 6L * 1024 * 1024 * 1024, "Insufficient disk space")`
— it needs over 6GB free to even attempt writing its fixture (a 1GB temp file
written via a 1024-iteration, 1MB-at-a-time loop, then copied 6 times into a
zip64 archive, i.e. ~7GB of real disk I/O in one test method). A HANG or FAIL
here can plausibly be that assumption's guard misfiring or the write itself
stalling under disk pressure, so this was checked directly rather than
assumed, per this session's instructions.

**Checked and ruled out for this run.** `Get-PSDrive C` on the Windows box
this suite ran on reports **~178GB free** (`191718924288` bytes) — nearly 30x
the 6GB the test's own assumption requires. The `.err.log` for this run
contains no `IOException`, no disk-related message of any kind, and no sign
the process ever got anywhere near the write-heavy test (see below — it
hangs during class initialization, before any test method runs). The disk-
capacity theory does not fit this run's evidence.

## Symptom

`loader/spring-boot-loader`'s
`org.springframework.boot.loader.zip.ZipContentTests` TIMED OUT at 300.012s
on the 2026-08-06 Windows full-suite run
(`craton-fullsuite-windows-20260806-s2/all-jit/logs/loader_spring-boot-loader.org.springframework.boot.loader.zip.ZipContentTests.{out,err}.log`).
`.out.log` is **0 bytes** — the JUnit launcher never printed anything, so the
stall is before or during test discovery/first-context setup, not inside any
individual test method (in particular, not inside the
`openWhenZip64ThatExceedsZipSizeLimitOpensZip` write loop itself). `.err.log`
(19 lines) shows the usual clinit-fixup boilerplate, then heavy, sustained GC
activity for the rest of the window:

```
01:05:53 [moving-young] fallback #1: reason=innermost-rbp-belongs-to-unguarded-callee
01:06:00 [moving-young] fallback #2: reason=innermost-rbp-belongs-to-unguarded-callee
01:06:08 [moving-young] fallback #3: reason=unregistered-jit-frame-on-stack
01:06:13 [moving-young] fallback #4: reason=unregistered-jit-frame-on-stack
01:06:19 [moving-young] fallback #5: reason=innermost-rbp-belongs-to-unguarded-callee
01:06:24 [moving-young] fallback #6: reason=unregistered-jit-frame-on-stack
01:06:30 [moving-young] fallback #7: reason=unregistered-jit-frame-on-stack
01:06:36 [moving-young] fallback #8: reason=innermost-rbp-belongs-to-unguarded-callee
01:06:37 old-gen mark: conservative root 0x2a400000000 is an INTERIOR word of the live object at 0x2a3fffff708+0x2018 ... pinning the containing object
01:07:15 old-gen mark: conservative root 0x2a400000000 is an INTERIOR word ... (same address, recurs)
01:07:15 old-gen mark: conservative root 0x2a410e63900 is an INTERIOR word ...
01:09:09 old-gen mark: conservative root 0x2a400000000 is an INTERIOR word ... (same address again)
01:09:32 [moving-young] fallback #16: reason=unregistered-jit-frame-on-stack
01:09:41 old-gen mark: conservative root 0x2a400000000 is an INTERIOR word ...
```

16 young-gen non-moving fallbacks and repeated old-gen conservative-root
interior-pointer pinning (the same address, `0x2a400000000`, recurs at least
4 times across a 4-minute window) — every young collection during this
window fails to compact, and the old generation keeps finding the same live
object rooted only by an interior pointer it cannot safely relocate. This is
sustained GC churn, not a clean idle wait.

## Not the disk-capacity artifact; likely throughput/GC pressure on an already-borderline-slow class

Cross-referencing every recorded run of this class across the suite's
history:

| Run | Result | Seconds |
|---|---|---:|
| `hotspot-baseline-20260717` shard2 | PASS | 81.9 |
| `craton-rerun-20260717` shard2 | **HANG** | 300.2 |
| `craton-rerun-20260723` shard4 | **HANG** | 300.0 |
| `craton-rerun-20260728` shard1 | PASS | 261.9 |
| `craton-fullsuite-20260731` shard2 | PASS | 285.9 |
| `craton-fullsuite-azure-20260802` | FAIL, 1 aborted | 149.2 |
| `craton-residual32-20260804-s2` | **CRASH** — `OutOfMemoryError: Java heap space (alloc_array length 8192)` | 156.8 |
| `craton-fullsuite-azure-20260805-s3` | **HANG** | 300.0 |
| `craton-residual21-20260805-s3` | PASS | 174.5 |
| `craton-nonpassed-20260806-s1` | PASS | 310.5 |
| `craton-fullsuite-windows-20260806-s2` (this run) | **HANG** | 300.0 |
| `craton-fullsuite-g1-20260807-s2` | **HANG** | 300.1 |
| `craton-fullsuite-zgc-20260807-s2` | **HANG** | 300.1 |

The pattern: HotSpot is fast and clean (82s). CratonVM's own clean PASSes
cluster **150-310s** — already 2-4x HotSpot and, in one case
(`craton-nonpassed-20260806-s1`), a PASS that took *longer* than this run's
300s HANG budget. One run outright **OOM'd** with a small allocation
(`alloc_array length 8192` — an 8KB array, not the multi-megabyte zip
buffers, meaning the heap was already essentially exhausted when that
allocation landed) at the default `-Xmx 2g`. That is direct evidence this
class runs right at the edge of `-Xmx 2g`'s capacity even when it doesn't
outright hang, and the GC log for this run — sustained non-moving fallbacks
plus repeated interior-pointer pinning preventing compaction — is exactly
the shape a generational collector under real memory pressure produces:
every young collection degrades to a non-compacting sweep, the old
generation can't reclaim the pinned object, and the effective usable heap
shrinks run over run rather than being reclaimed.

This is the same general shape as the already-documented `ZipContentTests`
15GB-disk gotcha's sibling concern for this class in this codebase's prior
sessions, but the mechanism here is **heap pressure from the class's own
large in-memory buffers** (the test fixture writes gigabytes of data through
1MB `byte[]` buffers in tight loops — see
`ZipContentTests.java:309-314,324-325` — and `ZipContent.open`/entry-reading
holds decompressed/mapped views over a multi-hundred-megabyte-plus zip64
archive), not disk I/O capacity. Both can plausibly coexist as separate
environmental sensitivities for this one class; this run's evidence points
at the heap/GC one specifically.

## Root cause: not confirmed

Not established this session whether:

1. This is purely a throughput gap (CratonVM's generational GC doing
   meaningfully more total work than HotSpot's collectors for this
   allocation pattern, in the same vein as the jOOQ/Quartz timeout docs filed
   alongside this one), which would mean the class is simply too slow for a
   fixed 300s/`-Xmx 2g` budget under concurrent full-suite load and
   occasionally OOMs or times out depending on host conditions and exact
   allocation timing; or
2. A specific GC defect (the recurring interior-pointer pinning at the same
   address, or the persistent non-moving-young fallback) is actively
   *leaking* memory that should be reclaimable, making the class
   progressively slower/more memory-constrained than it should be rather
   than just naturally heavy.

`audits/old-sweep-liveness.md` section 7 (referenced directly in the log
line) is the right starting point for (2) — it already documents the
conservative-interior-root-pinning mechanism as a known, intentional
correctness safeguard, not obviously a bug in itself, but its *frequency*
here (the same address recurring across 4+ minutes without ever being freed)
is worth checking against that doc's own expectations for how often a
genuinely-dead object should still be reachable only by a stale interior
root.

## Affected classes

- `loader/spring-boot-loader` — `org.springframework.boot.loader.zip.ZipContentTests`
