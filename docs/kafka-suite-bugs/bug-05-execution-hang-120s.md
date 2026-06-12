# Bug 05 — Interpreter hang (120 s watchdog) during test *execution*

**Severity:** High — these packages discover fine but hang while *running* tests.
`--nojit`, so interpreter-path (not the JIT discovery hang of bug 01).
HotSpot runs all of them clean.

**Affected packages (CVM `--nojit` HANG; HotSpot DONE):**
- `org.apache.kafka.common`
- `org.apache.kafka.common.compress`
- `org.apache.kafka.common.metrics`
- `org.apache.kafka.common.network`
- `org.apache.kafka.common.security.authenticator`

**Symptom:**
```
=== T19.H1 watchdog: deadline of 120s elapsed; requesting thread stack dumps ===
tid=0 depth=0 class=RunPkg method=main ...
tid=0 depth=1 ...SessionPerRequestLauncher.execute ...   <- execution phase, not discovery
```
The top frame is `Launcher.execute`, i.e. discovery finished and a test body (or
a `@BeforeAll`/setup) is spinning or blocked. Likely candidates by package:
- `common.compress` — codec round-trips (zstd/lz4/snappy) in a loop.
- `common.metrics` — time-driven/`MockTime` sensor windows.
- `common.network` — selector/socket loops (cf. existing ServerSocket gaps).
- `common.security.authenticator` — SASL handshake loops.

These may be distinct root causes; each needs its own bisection to the test class
and the spinning frame (re-run the package with a shorter watchdog and capture the
deepest interpreter frame).

## Status
- [x] Reproduced; confirmed CVM-only (HotSpot completes).
- [ ] Per-package frame localised / fixed (open).
