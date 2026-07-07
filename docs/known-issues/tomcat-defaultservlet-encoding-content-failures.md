# Tomcat `TestDefaultServletEncoding*` — functional failures (HTTP -1 / content mismatch), crash family retired

Status: open (untriaged)

Date observed: 2026-07-07 (Azure Linux, dev @ `c15cee62` + the
gc-blocked-mirror real-net bracketing fixes; real-JDK jdk25,
`CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1`)

## Context

These three classes were part of the 2026-06-29 hard-crash family
(`docs/internal/gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md`).
The crash family is retired: full 1360-case nojit runs now complete with zero
GC signal (no stale-pointer warnings, no panics). What remains is a large,
previously-masked functional failure cluster:

- `TestDefaultServletEncodingWithoutBom --nojit`: 1360 run, **264 failures**
- `TestDefaultServletEncodingWithBom --nojit`: 1360 run, **228 failures**
- jit runs: crash-free, hit the harness wall-clock cap mid-suite on the
  loaded shared host (587 and 990 cases respectively, no failures counted
  before the cap — jit failure count not yet measured to completion).

On HotSpot (jdk25) these classes pass.

## Failure shapes (from the run logs)

1. `expected:<200> but was:<-1>` — ~240 of the 264 WithoutBom failures. The
   test's HTTP GET returned no status line at all. First failing cases
   feature exotic output encodings (e.g.
   `testEncoding[58: … outputEnc[ibm850] … useWriter[false]]`) — check
   whether the distribution is charset-correlated or load/timing-correlated
   before trusting this hint.
2. `org.junit.ComparisonFailure` content mismatches (the remainder).

## Notes / candidate relatives

- `ibm850`/`cp850` was added as a *decode* alias in `6a04b0e3`; the *encode*
  path through the servlet Writer chain is unverified.
- The `StreamEncoder` real-mode shim family
  (`dohead-streamencoder-eager-flush-commit-threshold.md`, FIXED on dev) is
  already included in the tested build — these failures persist after it.
- Zero GC/staleness signal in all four logs; do not re-open the retired crash
  doc for these.

## Repro

```
cd apps/tomcat
CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  cratonvm --java-home <jdk25> --nojit -Xmx2g -cp "$(cat .suite/cp.txt)" \
  org.junit.runner.JUnitCore org.apache.catalina.servlets.TestDefaultServletEncodingWithoutBom
```
(full suite ≈ 1360 embedded-Tomcat boot/stop cycles — allow 30-60+ min under
load; per-case progress on stderr via `Starting test case [N: …]`)
