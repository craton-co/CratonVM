# Corpus runner

`regression-suite/run.sh` is 72 small deterministic vectors and currently sits at
69 passed / 2 failed. That is a useful gate and it **licenses almost nothing
about real applications**. Phase 2 of the JDK-only roadmap retires native
bridges *by family* and lets real applications adjudicate; without a real
corpus there is nothing to adjudicate against. This directory is that missing
piece.

```
bash regression-suite/corpus/run-corpus.sh list
bash regression-suite/corpus/run-corpus.sh info <corpus>
bash regression-suite/corpus/run-corpus.sh discover <corpus> [--limit N]
bash regression-suite/corpus/run-corpus.sh run <corpus> [options]
```

Exercised on this Windows host under Git Bash. A PowerShell arm was **not**
written because it was not needed: `regression-suite/run.sh` is already bash
and runs here, and so does this. See "Windows specifics" below for the two
things that had to be handled to make that true.

## What it is, and what it is not

It adjudicates **correctness**: pass / fail / diverge, against HotSpot 25.

It is **not a benchmark**. Wall-clock throughput on this shared, loaded host is
worthless — three clean interleaved A/B pairs have previously turned out to be
pure load drift. The results TSV carries `cv_ms` / `hs_ms` columns *only* so a
four-minute workload can be told from a four-second one when choosing a
timeout. **No throughput claim may be built on them.**

## HotSpot is the oracle

The oracle arm is HotSpot 25, never the other CratonVM mode. A
CratonVM-vs-CratonVM comparison shows self-consistency, not correctness: both
arms can be wrong in the same way and agree perfectly.

The oracle receives **neither** the mode flag **nor** `--cv-args`. Those are
CratonVM spellings and the reference run has to stay unmodified.

A HotSpot arm that started nothing is a **disagreeing precondition, not a
pass**. Two cases are detected and reported rather than scored:

| verdict | meaning |
|---|---|
| `ORACLE-UNUSABLE` | the oracle arm did not reach `RAN` — it crashed, timed out, or never started. Its output is an artefact of its own failure, so there is no ground truth. |
| `ORACLE-VACUOUS` | the oracle ran but reported `tests=0`, or every discovered test **aborted**. An `assumeTrue`/`Assumptions.abort` that disagreed with the host is not a green oracle. |

Both count as **unadjudicated**. A run in which *nothing* was adjudicated exits
2 and says so — "we learned nothing" must never render as "we verified it".

## Failure is reported in four distinct shapes

Never merged, because they have disjoint suspects:

| state | meaning |
|---|---|
| `NOSTART` | the workload class never began executing (no `CORPUS-START` marker). A classpath or launcher problem — **not a VM answer**. |
| `CRASH` | SIGSEGV / rust panic / fatal runtime error / access violation. |
| `TIMEOUT` | killed at the wall. |
| `RAN` | started and reached a terminal marker. |

**A `TIMEOUT` here is very often a SIGSEGV that printed no result line.** It is
reported as a hard failure to be diagnosed. It is never reported as slowness,
and it is never evidence about performance.

`--timeout` defaults to **600 s**. This host needs generous timeouts: one
existing suite vector takes ~4 m 55 s, and the 120 s default manufactures
phantom errors that read as independent defects.

## The wrapper, and why a bare exit code is not enough

Workloads run under `CorpusMain`, which prints `CORPUS-START` before entering
the target's `main` and `CORPUS-END` / `CORPUS-THROW` / `CORPUS-NOMAIN` after.

This is not ceremony. `apps/h2database-suite-runner/run-h2-suite.sh:174`
classifies a workload by exit code alone, and on this host that is provably too
weak: running `org.h2.test.unit.TestBitStream` on HotSpot 25 **exits 0 having
printed nothing at all**. `rc=0` with no output cannot distinguish "ran and
passed" from "never ran". The wrapper's markers are printed by bytecode the
workload's own VM executes, so they are a statement about that VM.

The payoff was immediate — see "What was actually run" below.

Only the markers (plus any `SBRUNNER_RESULT` line) form the comparison key.
Everything else a real application prints is timestamps, temp paths, thread
names and heap addresses, none of which two VMs can be expected to match and
all of which would manufacture divergences. A `CORPUS-THROW`'s **thrown type**
is in the key; its stack frames stay in the log.

## Windows specifics

Two things, both load-bearing:

1. **`@argfile`, always.** Windows caps a command line at 32767 characters and
   the hibernate-orm classpath alone is 40174 bytes. Inline it does not fail
   cleanly — it fails as a *truncated classpath*, i.e. as a fabricated linkage
   error. Both arms support argfiles: `java` since 9, and CratonVM at
   `vm-cli/src/main.rs:1132` (`expand_argfiles`). Paths are written with forward
   slashes and quoted, because both argfile grammars treat `\` as an escape.

2. **Native paths.** `MSYS_NO_PATHCONV=1` is required (as in `run.sh:41`) or
   Git Bash mangles every classpath, but it also means a `/c/...` path handed to
   `java.exe` arrives verbatim and is not a path. `$HERE` is resolved with
   `pwd -W`. The first run of this script died exactly this way:
   `error: file not found: \c\craton\...\CorpusMain.java`.

## Corpora

`apps/` is gitignored, so nothing here assumes a corpus is in version control.
Every corpus declares candidate roots and a `corpus_is_built` predicate, and
resolution is by **built output, never by directory existence**.

That distinction is not theoretical. Two H2 trees exist on this host; the one
under `C:/craton/apps` has a `target/classes` containing exactly one file
(`META-INF/versions/21/org/h2/util/Utils21.class`), no `org/h2/Driver.class`,
and an empty `target/test-classes`. A runner resolving by existence would pick
the decoy and report ~217 identical `NoClassDefFoundError`s that read as a
sweeping VM regression. Equally: the presence of a `*-suite-runner` directory
says nothing about whether the corpus is built — `apps/spring-boot` contains a
runner and no corpus at all.

| corpus | confidence | notes |
|---|---|---|
| `h2` | verified | built classes and dependency jars are in **different roots**; the classpath unions them. 217 concrete test classes discovered. |
| `spring-framework` | verified | 7.1.0-SNAPSHOT module jars. The two `*-repack-*.jar` are load-bearing. |
| `tomcat` | verified | **the ant build is not needed** — `output/` is already populated (2798 + 1881 classes, 35 jars). |
| `commons-math` | probable | classes verified present; JUnit arm not exercised. |
| `bc-java` | probable | classes verified present; JUnit arm not exercised. |
| `hibernate` | **blocked** | 183 of 241 classpath entries are **evicted gradle-cache jars**. Refuses loudly rather than composing a partial classpath. |

`confidence` is about *this lane's evidence*, not about code quality:
`verified` means the composition was exercised here; `probable` means the build
output was measured but no workload was run.

## What was actually run

Dry-run on 2026-08-12, H2, `org.h2.test.unit.TestBitStream`, against a **stale
2026-07-26 binary from the main tree** (this worktree has no `target/`, and this
lane does not build the VM). This validates the harness, **not** the VM:

```
HotSpot arm : CORPUS-START / CORPUS-END completed=true      rc=0
CratonVM arm: CORPUS-START ... then nothing                 rc=124 (killed at 300 s)
verdict     : CV-TIMEOUT
```

The wrapper paid for itself on the first run: the HotSpot arm had been silent
under a bare exit-code contract, and the CratonVM arm's `CORPUS-START` with no
terminal marker says the workload **started and then hung or died inside the
test** — which is a different and far more actionable finding than `rc=124`.

Treat that row as a harness result. It is not a verdict on any current build.
