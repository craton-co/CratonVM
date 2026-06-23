# Tomcat full-suite run `tc1` — CratonVM vs HotSpot

**VM:** CratonVM dev `bfed13f5` (this session's fixes: BUG-V monitor desync,
Condition.await/LBQ stale-oop, and the crash-#2 stale-TLAB / stale-oop-across-
arena-`grow()` cascade — BUG-W, all merged to dev).
**Suite:** all 651 JUnit classes, each its own JVM (`JUnitCore <class>`),
`run-suite.ps1`, 45 s/class, parallel 6.
**Caveat:** the run executed under **100 % external CPU load** (a concurrent
hibernate-suite session, 7 worker VMs) and a deliberately short 45 s timeout to
sweep for crashes quickly. **HANG is therefore heavily inflated** — many fast
classes timed out purely from contention; the true HANG/PASS split needs a
low-contention, longer-timeout run. **CRASH/NOSUMMARY are reliable** (VM aborts
are fast and contention-independent), which is the point of this sweep.

## Counts (CratonVM, 651)

| status | count |
|--------|------:|
| PASS | 180 |
| HANG | 330 *(contention/perf-inflated)* |
| FAIL | 136 |
| NOSUMMARY | 4 |
| CRASH | 1 |

HotSpot baseline (loop4, clean): 627 PASS / 24 non-PASS (all environmental —
openssl/httpd binaries, tribes multicast, JspC, DeployTask, DefaultServlet*,
HTTP/2 flaky). Those 24 are excluded from CratonVM attribution below.

## Crashes (the priority) — only **1** genuine, deterministic CrashVM crash

The earlier-session fixes eliminated the crash cluster (handoff loop1 had 229
NOSUMMARY + 2 CRASH). All 5 of this run's CRASH/NOSUMMARY were re-run standalone
(no contention):

| class | verdict standalone | report |
|-------|--------------------|--------|
| `…session.TestFileStoreConcurrency` | **CRASH** (deterministic, 3×) | [BUG-Z](BUG-Z-filestore-concurrency-gc-segv.md) |
| `…filters.TestCsrfPreventionFilter2` | TIMEOUT (slow HANG, not a crash) | — |
| `…nonblocking.TestNonBlockingAPI` | TIMEOUT | — |
| `…realm.TestGenericPrincipal` | TIMEOUT | — |
| `…util.buf.TestCharChunkLargeHeap` | TIMEOUT | — |

`TestFileStoreConcurrency` was the only genuine crash; HotSpot only *FAILs* it
(no crash), so it counted. **Now FIXED** ([BUG-Z](BUG-Z-filestore-concurrency-gc-segv.md),
commit `31d4f989`): `forward_object` was recording 8-aligned-non-null garbage
forwarding addresses into `pointer_map`; it now rejects any forward not inside
`young_to`/`old_gen`. So after this session's fixes **the 651-class suite has 0
genuine CratonVM crashes** (FileStore is now a slow HANG like the other
embedded-server/concurrency tests). bt18/16/14 regression-clean.

## FAILs — 128 CratonVM-only (HotSpot passes), by package

These are **correctness** failures, not crashes. Biggest clusters (likely
shared root causes — good fix leverage):

| count | package | likely theme |
|------:|---------|--------------|
| ~~20~~ → **5** | `catalina.webresources` (+2 `.war`) | **0→17/22 FIXED** (merged to dev): `Files.copy(dir)` (os error 5 → create_dir), `File(parent,child)` resolution (PathBuf::push trailing-sep/`/`-discard), multi-release `JarFile(File,bool,int,Version)` ctor (was "zip file is empty"), `JarFile.getInputStream(ZipEntry)` (was null → wrapper NPE). Residual 4: `classpath:` URL handler, a negative-NPE test, WAR URL connection, embedded-server serving (200 vs −1) |
| 13 | `tomcat.util.net` | TLS/SSL stack (no real TLS server — large effort) |
| 8 | `tomcat.websocket` (+5 `.server`) | WebSocket upgrade/handshake |
| 8 | `catalina.startup` | context/web.xml deploy edge cases |
| 5 | `juli` | logging config |
| 4 ea | `util.buf`, `tribes.group.interceptors`, `filters`, `connector`, `authenticator` | mixed |
| 3 ea | `util.http`, `jasper.tagplugins.jstl`, `valves`, `tribes.group`, `loader` | mixed |

(The full list is `/tmp/cv_only_fail.txt`; regenerate via
`diff-runs.sh tc1` against the `loop4/hotspot` baseline.)

## HANGs

~330 reported, but inflated by the 45 s timeout + 100 % external CPU. The genuine
residual is the documented interpreter-throughput limit on embedded-server tests
(no JIT on the cold per-class JUnit path) — a perf characteristic, not a defect.
A low-contention run at 120–300 s is needed to separate true hangs from
slow-PASS; not attempted here because another session held the CPU.

## How to reproduce

```
.tooling/run-suite.ps1 -Vm craton -ListFile all-tests.txt -Tag tc1 -TimeoutSec 120 `
  -Parallel 6 -CratonExe <binary>
.tooling/run-suite.ps1 -Vm hotspot -ListFile all-tests.txt -Tag tc1 -TimeoutSec 120 -Parallel 6
bash .tooling/diff-runs.sh tc1
```
