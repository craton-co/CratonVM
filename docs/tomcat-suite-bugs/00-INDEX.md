# CratonVM — Tomcat 12.0 suite bug groups

Bug groups found running the complete Apache Tomcat 12.0 unit suite (651 JUnit
classes) under CratonVM vs HotSpot. Harness: `apps/tomcat/.tooling/run-suite.ps1`
(gitignored, local). HotSpot baseline: **635 PASS / 14 FAIL / 1 HANG /
1 NOSUMMARY** (the 16 non-PASS are all environmental — openssl/httpd binaries,
tribes multicast, flaky HTTP/2 — and are NOT counted as CratonVM bugs).

This directory groups the issues by root cause rather than per-class. Several
earlier groups have been FIXED and merged to `dev`; the dominant remaining wall
is interpreter throughput on embedded-server deployment.

| # | Group | Status |
|---|-------|--------|
| [01](01-jsse-tls-chain-FIXED.md) | JSSE/TLS chain (SSLContext→live HTTPS) | **FIXED** (dev) |
| [02](02-native-young-gen-oom-abort-FIXED.md) | Native alloc young-gen OOM hard-abort | **FIXED** (dev) |
| [03](03-gc-root-snapshot-contention-FIXED.md) | GC root-snapshot lock contention / per-call cost | **FIXED** (dev, partial) |
| [04](04-embedded-server-throughput-wall-OPEN.md) | Embedded-server deployment throughput wall | **OPEN** (dominant) |
| [05](05-suite-rerun-fail-triage.md) | Suite-rerun FAIL set — to triage | OPEN (preliminary) |
| [06](06-openssl-ffm-clinit-segv-CRASH.md) | OpenSSL Panama/FFM clinit NPE → JIT SEGV | **OPEN** (CRASH) |
| [07](07-beanelresolver-property-not-found-FAIL.md) | BeanELResolver bean property not discovered | OPEN (FAIL) |
| [08](08-importhandler-standard-packages-npe-FAIL.md) | ImportHandler standard-package list NPE | OPEN (FAIL) |

Groups 06-08 are individually-diagnosed (real per-class repro + trace) from the
group-05 worklist; more will be added as triage continues.

## Current rerun status (with TLS/server env + `-Xmx2g`, 180s timeout)

The post-fix rerun (`results/rerun/craton`) — partial at time of writing
(~253/651 classes) — reads **51 PASS / 55 FAIL / 145 HANG / 2 NOSUMMARY**. The
**145 HANG dominate** and are overwhelmingly embedded-server classes hitting the
180s per-class timeout: that is the throughput wall (group 04), not 145 distinct
broken tests. The suite is **NOT green**; the gap is mostly throughput, plus the
group-05 FAILs still to be diagnosed.

**The suite is not green.** The TLS/JCA/GC crash-class bugs are fixed; the
remaining work is (a) interpreter throughput for server-test deployment and
(b) triaging the genuine per-class FAILs in group 05.
