# DoHead family — residual low-rate transport flake (OPEN, low severity, environmental)

**Status: OPEN, low severity, not scheduled for active investigation** unless
the rate climbs materially above the ~1-2%-per-class-run baseline measured
below, or a VM-side corruption/panic signature is ever observed alongside it.

Split out of
[`docs/internal/tomcat-08-07/dohead-post-fix-sporadic-residuals-FIXED.md`](../../internal/fixed-suite-bugs/tomcat/dohead-post-fix-sporadic-residuals-FIXED.md)
on 2026-07-21: that document tracked a long series of real, now-fixed
CratonVM correctness bugs (selector wakeup races, HashMap node-layout
corruption, an OSR exception-table gap, and two unpinned-receiver GC hazards
in `native_bos_flush_locked`/`native_map_remove_pinned`). After the last of
those fixes, a small residual remained: sporadic client-visible transport
failures on the 64-class `TestHttpServletDoHeadInvalidWrite*` family with
**no corresponding VM-side error, panic, or `gen_heap` corruption-guard hit**
in the same run. This document tracks that residual specifically, now that
it is isolated from any known correctness bug.

## Symptom

One of the following, on an otherwise-passing class, roughly 1-2% of
class-runs in a full 64-class two-process matrix:

- `java.net.SocketTimeoutException: Read timed out` (client `HttpURLConnection`
  read, `TomcatBaseTest.methodUrl`).
- `java.io.IOException: End of input stream with [9] bytes left to read`
  (HTTP/2 frame parser, `Http2TestBase$TestInput.fill`).
- `SocketException: Connection reset: Broken pipe`.
- A class-level `TIMEOUT` that does not reproduce on an immediate isolated
  rerun.

Never the deterministic header-count assertion (`expected:<N> but was:<N±1>`)
or an OSR/exception-table failure shape — those were distinct, already-fixed
bugs. Never accompanied by a `[cratonvm-vm] gen_heap::set_field: out-of-bounds
field write dropped` guard line, a Rust panic, or any other VM-internal
diagnostic in the same class's log.

## Why this is believed environmental, not a CratonVM defect

- **Present since the very first 2026-07-15 catalogue** of this family, at
  the same rate, including in baselines taken before any of this family's
  fixes landed — i.e. it did not appear or worsen as a side effect of any
  change made along the way.
- **Rate is consistent (~1-2% of class-runs) across every measurement**
  taken over the 2026-07-15 through 2026-07-21 investigation, on both busy
  and quiet hosts, with JIT on and with `--nojit` (interpreter-only)
  controls, and is uncorrelated with any specific test parameter
  (buffer size, reset type, writer vs. stream, valid/invalid write counts)
  — different measurements caught it on different, unrelated parameter
  combinations each time.
- **Zero corruption-guard hits.** The `gen_heap` out-of-bounds-write guard
  (which caught every one of this family's real bugs, going back to C29) has
  fired exactly zero times across every occurrence of this residual observed
  during the 2026-07-20/21 closure session — 192 class-runs' worth of logs
  inspected line-by-line, two failures, zero guard lines in either.
- **Correlates with concurrent host load.** The clearest recent occurrences
  (2026-07-21 C46 verification run) landed during a measured `uptime` load
  average of 6.3-6.8 on a 16-core box shared with dozens of other concurrent
  build/test sessions — consistent with the client's fixed read-timeout
  budget being exceeded by scheduling contention rather than any VM defect.

None of this *proves* there is no further CratonVM-side bug — see "If this
resurfaces" below for what would change that judgment — but the evidence
gathered so far is consistent with ordinary host/network scheduling jitter
on a heavily oversubscribed shared build box, not a correctness defect.

## Reproduction

Same harness as the parent investigation:

```bash
# Linux (Azure build host), 2-process/1g-heap stress oracle:
bash run-dohead.sh <cratonvm-exe> <outdir> 2 1g <passes> 900 TestHttpServletDoHead
```

```powershell
# Windows:
pwsh apps\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -RunName x -TimeoutSec 900 `
  -Parallel 2 -MaxHeap 1g -ClassFilter 'TestHttpServletDoHeadInvalidWrite'
```

Expect ~98-99% of classes clean per pass; the residual appears as isolated
287-288/288 singletons, never reproducing on an immediate isolated rerun of
the affected class.

## If this resurfaces at a higher rate, or with a corruption signature

1. Re-run with `CRATONVM_SOCKET_CAPTURE=<prefix>` (single worker, to avoid fd
   collisions across concurrent processes — see the parent doc's C46 section
   for the exact technique) and check the captured `.idx`/`.w.<fd>` files
   for the failing connection's wire trace.
2. Grep every log in the run for `gen_heap::set_field` — if it appears
   alongside a failure, this is NOT this doc's residual; it is a new
   instance of the unpinned-receiver-across-`invoke_virtual` hazard class
   (see the parent doc for the established fix pattern:
   `pin_native_root`/`read_native_pin`/`unpin_native_roots` around the
   receiver across any `invoke_virtual`/GC-capable call).
3. Re-measure the rate on a quiet host (`uptime` load comfortably under the
   core count) — if it stays at 1-2% even there, that raises this doc's
   priority; if it only appears under measured contention, it remains
   environmental.
