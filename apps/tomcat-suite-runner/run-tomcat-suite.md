# Tomcat suite runner for CratonVM (Linux) - `run-tomcat-suite.sh`

Linux counterpart to the Windows `run-tomcat-suite.ps1` harness (now tracked
alongside it — see §6 for its classpath contract). Runs the Apache Tomcat JUnit test suite
one-process-per-class against CratonVM or a real-JDK (HotSpot) baseline,
sharded N ways, with a resumable per-shard `results.csv`.

## 1. Prerequisites

Needs a Tomcat checkout with:
- compiled test classes under `$TC_ROOT/output/testclasses`,
- a full `CATALINA_BASE` under `$TC_ROOT/output/build` (i.e. `ant deploy`'s
  output: `conf/`, `webapps/{ROOT,examples,manager,host-manager}`, `bin/`,
  `lib/`, `logs/`, `temp/` - **not just the compiled classes**, or every
  `TomcatBaseTest`-based class fails fast with `FileNotFoundException:
  .../conf/logging.properties` cascading into `A child container failed
  during start`),
- a flat classpath file (jars + compiled-class dirs, `:`-joined) at
  `$TC_ROOT/.suite/cp-linux-fixed.txt`. It **must include Ant's own
  `ant.jar` + `ant-launcher.jar`**: `org.apache.catalina.ant.TestDeployTask`
  drives Tomcat's `DeployTask` Ant task and `org.apache.jasper.JspC` (under
  test in `org.apache.jasper.TestJspC`) extends `org.apache.tools.ant.Task`,
  so without them both classes die with `NoClassDefFoundError:
  org/apache/tools/ant/Task` on *either* VM — a fixture gap that is easily
  misread as a CratonVM regression. The Windows `.ps1` harness's
  `Build-Classpath` adds them automatically; the Linux file is hand-built, so
  append `/usr/share/java/ant.jar` + `/usr/share/java/ant-launcher.jar`
  (`apt-get install ant`) by hand,
- a class list at `$TC_ROOT/.suite/all-tests.txt` (one FQCN per line, **no
  CRLF** - if generated on Windows and `scp`'d over, run
  `sed -i 's/\r$//' all-tests.txt` first or every class name fails to resolve),
- an **Apache httpd binary** for the 9 `org.apache.tomcat.integration.httpd.*`
  classes. Each of them starts its own httpd reverse proxy in front of the
  embedded Tomcat under test; with no binary they all fail with a
  connection-refused to the proxy port, on HotSpot exactly as on CratonVM.
  The script passes `-Dtomcat.test.httpd.path="$HTTPD_PATH"`, defaulting to
  `command -v httpd`. Debian names the binary `apache2`
  (`apt-get install apache2`), so either symlink it onto `PATH` as `httpd` or
  set `HTTPD_PATH=/usr/sbin/apache2`. On Windows there is no httpd at all:
  run `pwsh apps/tomcat-suite-runner/setup-httpd-windows.ps1`, which unpacks a
  SHA-256-verified Apache Lounge build into a local directory (no service, no
  `PATH` change) and applies `fixtures/httpd-ready-timeout.patch` - upstream
  `TesterHttpd` allows httpd 1000 ms to bind its listener, ample on Linux but
  consistently short of the 1.0-1.5 s that MPM WinNT startup measures, so on
  Windows all 9 fail ~1 s in even with a good httpd installed.

None of this setup is automated by the script itself (mirrors the `.ps1`
harness's `-Setup` step, which isn't reproduced here) - reuse an existing
fixture, or run `ant deploy && ant test-compile` in a real Tomcat checkout and
point `TC_ROOT`/`CP_FILE`/`CLASSLIST` at it.

## 2. Usage

```sh
export CRATONVM_EXE=/path/to/cratonvm-<unique-name>
export TC_ROOT=/data/data/apps/tomcat        # or wherever the fixture lives

# One shard:
./run-tomcat-suite.sh craton 0 6 my-run

# All 6 shards, craton:
for i in 0 1 2 3 4 5; do
  ./run-tomcat-suite.sh craton $i 6 my-run &
done; wait

# HotSpot control pass over the same (or a reduced) class list:
for i in 0 1 2 3 4 5; do
  ./run-tomcat-suite.sh hotspot $i 6 my-run-hotspot &
done; wait

# Rerun only a subset (e.g. everything that didn't PASS the first time):
awk -F, '$4!="PASS"{print $1}' \
  "$TC_ROOT"/.suite/results/my-run/shard-*/results.csv | sort > /tmp/rerun.txt
./run-tomcat-suite.sh craton 0 1 my-run-retry /tmp/rerun.txt
```

## 3. Output layout

```
$TC_ROOT/.suite/results/<run_name>/shard-<i>/
  results.csv       class,rc,seconds,status,loadavg1   (append-only, RESUMABLE)
  <fqcn>.log        full stdout+stderr, kept only for non-PASS classes
  DONE              marker written when the shard's class list is exhausted
```

Status values: `PASS` (JUnit `OK (n tests)`), `FAIL` (`FAILURES!!!`), `HANG`
(hit the per-class timeout), `CRASH` (panic/SIGSEGV/SIGABRT fingerprint in the
log), `NOSUMMARY` (process exited with no JUnit summary at all).

`loadavg1` is the host's 1-minute load average as that class finished. It is
there because `HANG` is not self-explanatory on a shared box: a class capped at
`TIMEOUT_SEC` is recorded `HANG` whether it is stuck or merely starved, and the
two are indistinguishable from the status alone. Read it before reading a
`HANG` — and before reading a `FAIL` from any class that asserts on timing.

## 4. Never trust a FAIL/HANG count without a same-fixture HotSpot baseline

A large chunk of "failures" in a fresh Linux fixture are typically missing
test infrastructure (no `httpd` binary for `integration.httpd.*`, no
OCSP-responder lock dir, no OpenSSL wiring, `*LargeHeap` classes needing a
bigger `-Xmx` than the flat default) rather than CratonVM bugs - these fail
identically under real JDK 25 in the same fixture. Always run the `hotspot`
mode over the same class list first (or alongside) and diff: only classes
that **PASS on hotspot but FAIL/HANG/NOSUMMARY/CRASH on craton** are candidate
regressions. See `tomcat-suite-bugs/16-full-suite-6shard-rerun-20260721.md`
for a worked example (646-class run, 23 confirmed regressions out of 196
initial non-PASS classes; the other 172 failed on HotSpot too).

## 5. Known gotchas

- `cratonvm`'s classpath flag is `-c`/`--classpath`, not `-cp` (that's plain
  `java`'s spelling) - the script already handles this per-mode.
- `--java-home` does not, by itself, force real-JDK mode off; CratonVM
  auto-detects and boots a real JDK whenever one is on `PATH`/`--java-home`.
  This runner always points `--java-home` at a real JDK 25 (real-JDK mode is
  what we want to test against Tomcat), so no `--synthetic-jdk` flag is
  passed.
- `nohup`'d background shard launches on a shared host get killed at SSH
  logout unless linger is enabled first: `sudo -n loginctl enable-linger
  $(whoami)`.

## 6. Windows harness: the classpath must be COMPLETE, and it now says so

`run-tomcat-suite.ps1 -Setup` builds `apps\tomcat\.suite\cp.txt` from
`Build-Classpath`. Two things about it are load-bearing:

- **Each jar is looked up by several names.** Tomcat's `ant download-compile`
  renames what it downloads (`bouncycastle-provider-1.84.jar`), while the
  Gradle/Maven caches hold the upstream artifact name
  (`bcprov-jdk18on-1.84.jar`). Matching only the renamed name is what silently
  dropped BouncyCastle and EasyMock off this box's classpath for weeks —
  eleven classes reported `NoClassDefFoundError` and were filed as two
  separate "CratonVM" known-issues docs. Roots searched, in order:
  `C:\Users\Victor\tomcat-build-libs`, the Gradle module cache,
  `~\.m2\repository`, and `apps\tomcat\.suite\lib`.
- **A miss is now fatal.** Unresolvable jars are printed in red, written to
  `.suite\cp-missing.txt`, and abort the run unless `-AllowMissingLibs` is
  passed. Drop the jar into `apps\tomcat\.suite\lib` (any layout) and re-run
  `-RefreshClasspath`, which rebuilds `cp.txt` + `all-tests.txt` in seconds
  without the ~20 min `ant deploy && ant test-compile`.

`run-one.ps1` reads the same `cp.txt` and mirrors `Invoke-Mode`'s `$jvmArgs`
verbatim (4 × `--add-opens`, the `tomcat.test.*` system properties,
`--nojit`/`-Xint`). Keep the two lists in step: a single-class repro that omits
`--add-opens=java.base/java.lang` fails every EasyMock-based class for reasons
that have nothing to do with the VM.

**EasyMock on JDK 25 fails on HotSpot, by design of neither.** EasyMock 5.6.0
class-mocking needs byte-buddy's `ClassInjector.UsingUnsafe`, which is
unavailable on JDK 25 under any flag combination; it falls back to a
`MethodHandles.lookup()` rooted in its own package and dies with "must be
defined in the same package as `org.easymock.internal.ClassProxyFactory`".
Eight classes (`TestSSLValve`, `TestJNDIRealm`, `TestPersistentManager`,
`TestWebappServiceLoader`, `TestCrawlerSessionManagerValve`,
`TestLoadBalancerDrainingValve`, `TestRequest`, `TestTldScanner`) are therefore
permanently red in a HotSpot control run and green under CratonVM — expected,
not a regression. Details:
`fixed-suite-bugs/tomcat/bouncycastle-easymock-classpath-fixture-gap-FIXED.md`.
