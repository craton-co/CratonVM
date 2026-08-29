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

## Failure is reported in eight distinct shapes

Never merged, because they have disjoint suspects:

| state | meaning |
|---|---|
| `NOSTART` | the workload class never began executing (no `CORPUS-START` marker). A classpath or launcher problem — **not a VM answer**. |
| `CRASH` | the LOG carries crash evidence: rust panic / fatal runtime error / access violation text. |
| `SIGNAL` | the EXIT STATUS says it died: 128+N, or a Windows NTSTATUS such as `0xC0000005`. Decoded in the `cv_status` column. |
| `TIMEOUT-STALLED` | killed at the wall having printed **nothing** for a long time. Hung, not slow. |
| `TIMEOUT-BUSY` | killed at the wall while **still writing**. Slower than this one cap — which is not the same claim as "hung". |
| `TIMEOUT-UNKNOWN` | killed at the wall; output silence could not be measured. |
| `LAUNCH-FAILED` | `timeout`/the binary never ran (125/126/127). A **harness** fault; scored `HARNESS-ERROR`, never against the VM. |
| `RAN` | started and reached a terminal marker. |

**A timeout on this VM is very often a SIGSEGV that printed no result line**,
which is why `SIGNAL` is separated from the wall at all, and why the wall itself
is split by whether the arm was still producing output when it was killed. The
`*_silent_s` column is that evidence. **A single cap cannot separate "hung" from
"slower than the cap"** — a `TIMEOUT-BUSY` row says so on the row and asks for a
re-run at 3x the cap rather than forcing a verdict. No `TIMEOUT-*` row is ever
evidence about performance.

Every `TIMEOUT` row recorded **before** 2026-08-12 is a `TIMEOUT-UNKNOWN` and
cannot be promoted from stored logs: the split needs the kill time, and no run
before that date recorded it. Re-adjudicating an old run therefore relabels
those rows and resolves nothing about them — that requires VM time.

## Verdicts

| verdict | bucket | meaning |
|---|---|---|
| `AGREE` | agree | both arms reached `RAN`, comparison keys identical, exit statuses identical. |
| `DIVERGE` | diverge | keys differ, **or** keys match and the exit statuses do not. |
| `CV-<state>` | broken | the oracle was usable and the CratonVM arm did not reach `RAN` (`CV-NOSTART`, `CV-SIGNAL`, `CV-TIMEOUT-STALLED`, …). |
| `ORACLE-UNUSABLE` | unadjudicated | the oracle arm did not reach `RAN`. No ground truth. |
| `ORACLE-VACUOUS` | unadjudicated | the oracle ran but learned nothing: `tests=0`, every discovered test aborted, a discovery-time throw with no `SBRUNNER_RESULT`, or a linkage-family throw. |
| `UNADJUDICATED` | unadjudicated | `--no-oracle`. |
| `HARNESS-ERROR` | harness | this script or this host failed: a launcher fault (125/126/127), or an arm that reached `RAN` with an **empty** comparison key. Exit 2. Never scored against the VM. |

A run that adjudicated **nothing** exits 2. Quote the denominator when quoting a
ratio: "11 of 13 rows agree" and "11 agree, 2 diverge, 10 never ran" are
different claims, and both have been published here as the first.

## The comparison key excludes the shutdown-hook END marker

`CorpusMain` prints `CORPUS-END <c> completed=true` from the workload's own
bytecode on the normal return path, and `CORPUS-END <c> completed=exit` **from a
shutdown hook**. CratonVM registers shutdown hooks and never runs them (W7-27),
and every `junit`-kind workload exits through `System.exit`, so the hook line
appears on HotSpot and never on CratonVM — in every junit row of every corpus.
Keying on it manufactured **62 of the 75 `DIVERGE` rows** across the stored
corpus runs (measured; see
`docs/known-issues/jdk-only/C8-CORPUS-HARNESS-DEFECTS-20260812.md` §2.1 and the
C17 re-adjudication in `P4A-CORPORA-20260812.md` §R).

Only the `completed=exit` line is dropped. `completed=true` stays in the key —
it is a real statement by the workload's own VM — and the asymmetry is still
reported, as a note on the row. Do **not** "fix" this by stripping the
`completed=` qualifier: that leaves a bare `CORPUS-END` present on one side
only (so the row still diverges) and erases the one signal that tells a
bytecode-sourced marker from a hook-sourced one.

## The driver proves it can go red

`bash run-corpus.sh selfcheck` is a standing positive control over the real
classification/comparison/adjudication code, including an end-to-end pair that
runs `cmd_run` itself against HotSpot with a stub VM — once agreeing (must exit
0) and once diverging (**must exit 1**). Run it before believing a red corpus,
and after editing this driver.

```
32 unit + 6 end-to-end = 38 assertions; measured 38/38 pass, exit 0, on
2026-08-12 (lane C17).
```

`--unit-only` skips the ~40 s end-to-end half and says, in its own output, that
the unit half alone **cannot** prove the driver can go red. Precedent and
rationale: `scripts/check-no-diag-prints.sh`, and
`docs/known-issues/jdk-only/C8-CORPUS-HARNESS-DEFECTS-20260812.md`.

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
(`../../apps/META-INF/versions/21/org/h2/util/Utils21.class`), no `org/h2/Driver.class`,
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
