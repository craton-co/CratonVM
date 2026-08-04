# Spring Boot loader residual closure (2026-07-23)

**Status: OPEN — REGRESSED 2026-08-04.**

## Scope

The 15-class `loader/spring-boot-loader` residual shard exercised nested jar
URLs, archive/launcher class loading, security metadata, nested file systems,
and large ZIP64 content.  It had both functional residuals and a whole-class
timeout in `ZipContentTests`.

## Fix

The VM now keeps the Spring Boot loader's defining-loader and URL/archive
semantics on the native paths, preserves the standard closed-file contract,
and uses bounded native ZIP/file-data fast paths for the large streamed ZIP
fixtures.  The fast paths fall back to the Java implementation for unsupported
buffer layouts and for a closed `FileDataBlock`; this retains the original
exception behavior while avoiding repeated host file reads.

## Verification

Using JDK `25.0.3.9-hotspot`, r171 of the dedicated CratonVM binary built
after merging current `origin/dev`, and the
Spring Boot fixture root `C:\craton\CratonVM-spring-boot-rerun-20260717\apps\spring-boot`:

| mode | classes | failures | aborts | skips | container failures | wall time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| no-JIT | 15/15 PASS | 0 | 0 | 0 | 0 | 138.160 s |
| JIT | 15/15 PASS | 0 | 0 | 0 | 0 | 109.571 s |

`ZipContentTests` completed all 29 tests in 119.1 s no-JIT and 92.7 s JIT,
both inside the required 300-second per-class timeout.  This closes the
residual 15-class loader shard; no unresolved issue document was present to
move from `docs/known-issues`.

## Regression note (2026-08-04)

`org.springframework.boot.loader.zip.ZipContentTests` regressed twice over
successive residual rounds, both times centered on the same method,
`nestedZip64CanBeRead` (a 65,537-entry ZIP64 fixture written then read back
entry-by-entry):

- **2026-08-02** full-suite run (`craton-fullsuite-azure-20260802`):
  `ZipContentTests` reported **FAIL** at 149.2 s — 28/29 tests successful,
  **1 test aborted**, 0 tests failed. The `.err.log` shows only routine
  `moving-young` non-moving-sweep fallback warnings
  (`reason=innermost-rbp-belongs-to-unguarded-callee`) and a clean
  `System.exit(0)`; no exception/timeout detail survives in the captured
  logs to say why the one test aborted, but the whole-class time (149.2 s)
  is well inside the 300 s per-class ceiling this class was closed against.

- **2026-08-04** residual round (`craton-residual32-20260804-s2`): the SAME
  class now **CRASHES** at 156.8 s. The `.err.log` shows a long, escalating
  run of `moving-young` fallbacks — `reason=unregistered-jit-frame-on-stack`
  climbing through fallback #1 → #128 (i.e. the copying young GC never
  compacts and keeps falling back to non-moving free-list allocation) —
  ending in an uncaught, propagated-to-`main`
  `java.lang.OutOfMemoryError: Java heap space (alloc_array length 8192)`
  thrown from inside `ZipContentTests.nestedZip64CanBeRead` (via
  `AbstractInputStreamAssert.assertHasContent`). Because the OOM escapes
  `SbRunner.main` uncaught, the `.out.log` for this round is **empty** — no
  `SBRUNNER_RESULT` summary line at all, vs. 28/29 tests completing cleanly
  two days prior. This is why the harness reclassified the symptom from
  FAIL (with 1 abort) to CRASH: the same test went from borderline
  (timing out or hitting some soft limit) to outright exhausting the heap
  and killing the whole run.

Net: this is a **regression** of the 2026-07-23 closure above, which
verified `ZipContentTests` at 29/29 clean in both JIT and no-JIT modes with
no aborts. The persistent, monotonically-worsening `unregistered-jit-frame-on-stack`
non-moving-young fallback pattern strongly suggests the regression is in the
young-generation copying GC's root-map completeness for JIT frames (see
`INDEX_gc_notes` / moving-young fallback family) — under the sustained
small-object allocation pressure of a 65,537-entry ZIP64 fixture, the
inability to compact eventually exhausts the heap. Root cause not
conclusively isolated to a single commit in this pass; flagged for GC
follow-up rather than assumed identical to any single prior fix.
