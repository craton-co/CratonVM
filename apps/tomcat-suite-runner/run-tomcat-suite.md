# Tomcat suite runner for CratonVM (Linux) - `run-tomcat-suite.sh`

Linux counterpart to the Windows `run-tomcat-suite.ps1` harness (local to the
Windows box, not git-tracked). Runs the Apache Tomcat JUnit test suite
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
  `$TC_ROOT/.suite/cp-linux-fixed.txt`,
- a class list at `$TC_ROOT/.suite/all-tests.txt` (one FQCN per line, **no
  CRLF** - if generated on Windows and `scp`'d over, run
  `sed -i 's/\r$//' all-tests.txt` first or every class name fails to resolve).

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
  results.csv       class,rc,seconds,status   (append-only, RESUMABLE)
  <fqcn>.log        full stdout+stderr, kept only for non-PASS classes
  DONE              marker written when the shard's class list is exhausted
```

Status values: `PASS` (JUnit `OK (n tests)`), `FAIL` (`FAILURES!!!`), `HANG`
(hit the per-class timeout), `CRASH` (panic/SIGSEGV/SIGABRT fingerprint in the
log), `NOSUMMARY` (process exited with no JUnit summary at all).

## 4. Never trust a FAIL/HANG count without a same-fixture HotSpot baseline

A large chunk of "failures" in a fresh Linux fixture are typically missing
test infrastructure (no `httpd` binary for `integration.httpd.*`, no
OCSP-responder lock dir, no OpenSSL wiring, `*LargeHeap` classes needing a
bigger `-Xmx` than the flat default) rather than CratonVM bugs - these fail
identically under real JDK 25 in the same fixture. Always run the `hotspot`
mode over the same class list first (or alongside) and diff: only classes
that **PASS on hotspot but FAIL/HANG/NOSUMMARY/CRASH on craton** are candidate
regressions. See `docs/internal/tomcat-suite-bugs/16-full-suite-6shard-rerun-20260721.md`
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
