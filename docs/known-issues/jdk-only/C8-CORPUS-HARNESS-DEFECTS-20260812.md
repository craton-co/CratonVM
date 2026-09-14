# C8 — the corpus driver was the defect: nine harness faults, and which results they invalidate

**Date:** 2026-08-12 **Lane:** C8 (harness only) **File owned:**
`regression-suite/corpus/run-corpus.sh`

**This lane never built or ran the VM.** Everything below was measured with
`java`, `javac`, stub executables standing in for `cratonvm.exe`, and the
**stored logs of the nine corpus runs already on disk** under
`regression-suite/corpus/out/` (gitignored, present on this host). Every change
outside `run-corpus.sh` is a NOMINATION and is listed in §6.

---

## 0. Headline

**62 of the 75 `DIVERGE` rows in the nine stored corpus runs — 83% — were the
harness, not the VM.** They come back `AGREE` with byte-identical test counts
and identical exit statuses on both arms once one hook-sourced marker line
leaves the comparison key. Not one `AGREE` row moves the other way.

The count is not the damage. In the worst single run **11 of 13 `DIVERGE` rows
were the artefact, which buried the two genuine divergences among eleven fake
ones.** A harness that cries wolf eleven times in thirteen is worse than one
that reports nothing, because a reader who spot-checks two rows, finds both
fake, and stops checking has been actively misled.

Second headline, and the more dangerous shape: **a corpus run that adjudicated
nothing exited 0 and printed `AGREE=0 DIVERGE=0 CV-BROKEN=0 UNADJUDICATED=0`** —
a green summary for zero evidence. Measured, reproduced, fixed (§2.2).

---

## 1. The `rundir: unbound variable` report — what it actually was

Handed to this lane as: *"`line 549: rundir: unbound variable` fires on ANY run
that exits non-zero (`local rundir=` is declared at ~line 420 inside `cmd_run`;
line 549 is a `return 1`)"*, with an instruction to verify the line numbers.
Verified. **It is not a live bug in any committed version of the file, and the
two line numbers cannot both come from the same version.**

| version | line 420 | line 549 | `$rundir` expansions |
|---|---|---|---|
| `f0a472dcf` (old) | `local rundir="$OUT/$name-$MODE-$stamp"` | `return 1` | 420–423, 452, 533 — **all inside `cmd_run`, all after the declaration** |
| `0c0d56c18` (new, +14 lines) | — | (blank) | 434–437, 466, **547 `} > "$rundir/$safe.diff"`** |

The report's *declaration* line and its *`return 1`* line are both from the OLD
file. The only `$rundir` **expansion** anywhere near line 549 is at line **547
of the NEW file** — a redirection which, executed outside `cmd_run`, has no
local binding and under `set -u` says exactly `rundir: unbound variable`.

So the message is a NEW-file line number attached to an OLD-file execution.
bash reads a script incrementally from a file descriptor: when the file is
rewritten under a running interpreter, bash continues at its saved byte offset
and executes misaligned text. The two commits differ by exactly the 14 lines
that map 420→434 and 549→563. This is the same event as the two "transiently
unparseable at lines 549 and 591" observations from the same wave.

Reproduction attempts on the current file, both arms driven by stub
executables, **all clean**:

| forced path | result |
|---|---|
| CratonVM arm exits 1, oracle RAN → `CV-NOSTART`, `return 1` | exit 1, no error |
| `--no-oracle` → nothing adjudicated, `return 2` | exit 2, no error |
| arm killed by SIGSEGV, by the wall, and by a bad interpreter | exit 1 / 1 / 2, no error |

### What was done about it anyway

The mechanism is real even though the bug is not. Three mitigations, all in
`run-corpus.sh`:

1. **The executable tail is now one line.** Everything is a function
   definition; the entry point is `main "$@"` on the last line of the file. The
   window in which a rewrite can change what bash executes next is as small as
   a single-file bash script allows.
2. **Every run directory gets `run-corpus.sh.snapshot`** and the TSV header
   carries `# driver=run-corpus.sh sha256:<16 hex>`. A future weird failure can
   be compared against the exact text that produced the TSV, and two runs can be
   proven to have used the same driver.
3. Stated in the file: **do not edit `run-corpus.sh` while a run is in flight.**

---

## 2. The nine defects

Ordered by how much evidence each one falsified.

### 2.1 A shutdown-hook marker inside the comparison key (62 fake divergences)

`CorpusMain` prints `CORPUS-END <c> completed=true` on the normal return path
(bytecode the workload's own VM executed) and `CORPUS-END <c> completed=exit`
**from a shutdown hook**, for the `System.exit` path. CratonVM registers
shutdown hooks and never runs them (W7-27), and every `junit`-kind workload
exits through `System.exit` because `SbRunner` does. So that line appears on
HotSpot and never on CratonVM — in **every junit-kind row of every corpus**.

Measured over the stored runs, re-adjudicating each row's logs with the fixed
key:

| run | DIVERGE rows | → AGREE | remaining |
|---|---|---|---|
| `bc-java-jdk-only-…-205739` | 9 | **9** | 0 |
| `bc-java-jdk-only-…-210742` | 9 | **9** | 0 |
| `bc-java-jdk-only-…-213333` | 13 | **7** | 6 |
| `commons-math-jdk-only-…-204935` | 6 | **4** | 2 |
| `commons-math-real-jdk-…-205438` | 4 | **4** | 0 |
| `spring-framework-jdk-only-…-204553` | 27 | **26** | 1 |
| `spring-framework-jdk-only-…-211153` | 3 | **3** | 0 |
| `spring-framework-jdk-only-…-215547` | 1 | 0 | 1 |
| `h2-jdk-only-…-195247` (main-kind) | 2 | 0 | 2 |
| `h2-real-jdk-…-203313` (main-kind) | 1 | 0 | 1 |
| **total** | **75** | **62** | **13** |

Both `h2` runs are `main`-kind: their workloads return normally, so
`completed=true` is printed by bytecode on both arms and nothing flips. That is
the control — the fix touches exactly the rows it should.

**The previously-landed fix for this was insufficient, and measurably so.**
`P4A-CORPORA-20260812.md` NOMINATION 2 prescribed stripping the ` completed=…`
*qualifier* (`sed 's/^\(CORPUS-END .*\) completed=.*$/\1/'`); what landed was
the equivalent `sed 's/ completed=[A-Za-z]*//'`. Run on the real logs it still
diverges, because it leaves a bare `CORPUS-END SbRunner` line present on
HotSpot and absent on CratonVM:

```
current arm_key, HS: CORPUS-START SbRunner / SBRUNNER_RESULT tests=26 … / CORPUS-END SbRunner
current arm_key, CV: CORPUS-START SbRunner / SBRUNNER_RESULT tests=26 …
                                                        VERDICT = DIVERGE (still)
```

It was also wrong in direction: erasing the qualifier destroys the
`true` vs `exit` distinction, which is the **only** thing that tells a
bytecode-sourced line from a hook-sourced one.

The landed fix instead drops **only the hook-sourced line**:

```sh
| sed '/^CORPUS-END .* completed=exit$/d'
```

`completed=true` is kept, so the same nomination's own caveat — *"do not simply
drop `CORPUS-END`: its presence still distinguishes 'reached a terminal marker'
from 'died silently'"* — is honoured for the path where the marker means
anything. Three further guards make the narrow version safe:

* the **`SBRUNNER_RESULT` counts** remain in the key, and for a junit-kind
  workload they, not the END marker, are the terminal evidence;
* **exit statuses are now compared** when the keys match, so an arm that exits
  early cannot hide behind the dropped line (measured: across all nine runs, no
  row that agrees on markers disagrees on status, so this manufactures nothing
  today);
* the asymmetry is still **reported**, as a note on the row:
  `[hook-END on HotSpot only: CratonVM runs no shutdown hooks — excluded from
  the key, tracked separately]`. The VM gap stays visible; it just stops being
  counted 62 times.

### 2.2 A run that adjudicated nothing exited 0

With `--classes-from` pointing at a file of only comments, the effective
workload list was empty, the loop body never executed, and the summary printed
`AGREE=0 DIVERGE=0 CV-BROKEN=0 UNADJUDICATED=0` followed by **exit 0**. Every
"nothing was adjudicated" guard in the file keyed on `unadj > 0`, which is 0
when nothing ran at all.

Fixed three ways: the effective list is refused when empty (`exit 2`), a row
counter refuses a run with zero rows, and a new `HARNESS-ERROR` bucket exits 2.
Verified before (exit 0) and after (exit 2).

### 2.3 A harness path fault wearing a corpus verdict

`--out /c/Users/…/scratch` (a POSIX path, which is what Git Bash tab-completion
and every other script on this host produce) put `cp.args` at a path
`java.exe` cannot open. Both arms answered
``Error: could not open `/c/Users/…/cp.args'``, both scored `NOSTART`, and the
row was reported as **`ORACLE-UNUSABLE`** — a harness path bug rendered as a
statement about the fixture. Measured, then fixed two ways:

* `native_dir` normalises `--out` through `pwd -W` before it is used;
* a **preflight** runs `"$HS" "@$argf" -version` before any workload and exits
  2 as a precondition failure if the oracle cannot read the argfile. Its
  by-product is provenance: the TSV header now carries the oracle's own version
  line (`# oracle=openjdk version "25.0.3" 2026-04-21 LTS`).

### 2.4 A launcher fault scored as a VM failure

`timeout`'s own statuses — 125 (bad interval), 126 (not executable), 127 (not
found) — fell through to `NOSTART`, whose note sends the reader to the corpus
classpath. A non-numeric `--timeout` therefore produced 125 for **every**
workload and read as a corpus-wide launcher defect.

Now: 125/126/127 with no `CORPUS-` marker in the log ⇒ state `LAUNCH-FAILED` ⇒
verdict `HARNESS-ERROR`, counted in its own bucket, exit 2, never scored against
the VM. `--timeout abc` is refused as a usage error (exit 3). Both exercised
through the real `cmd_run`.

### 2.5 An empty comparison key would have reported a green corpus

If the key extractor ever matches nothing — a typo in the marker alternation, a
renamed marker — every comparison becomes `"" = ""` and **the entire corpus
reports AGREE**. This is the `check-no-diag-prints.sh` shape (a search that
fails and a clean tree producing the same output) in the worst possible place.

An arm that reached `RAN` must have printed `CORPUS-START`, which is in the key,
so an empty key is impossible unless the extractor is broken. It is now a
`HARNESS-ERROR` whose note says *"do not read this row, and do not read any
AGREE in this run."*

### 2.6 `TIMEOUT` collapsed three different findings

Its own note said *"a timeout is very often a SIGSEGV that printed no result
line"* — and then reported that suspicion as the same state as a slow test.
Full taxonomy and the evidence in
**`C8-CORPUS-TIMEOUT-TAXONOMY-20260812.md`**. Summary: `SIGNAL` (128+N or a
Windows NTSTATUS, decoded), `TIMEOUT-STALLED` (silent at the wall),
`TIMEOUT-BUSY` (still writing at the wall), `TIMEOUT-UNKNOWN`, `LAUNCH-FAILED`,
plus `137` no longer counted as a timeout — plain `timeout` never produces it,
so a 137 is somebody else's SIGKILL.

Two consequences of the old collapse are visible in the stored runs: a `CRASH`
row's note was produced by grepping the log for crash text, so a crash detected
purely from `rc >= 128` got an **empty** note; and every `CV-TIMEOUT` row
carried the same boilerplate regardless of whether the arm had been silent for
nine minutes or was printing when it was killed.

### 2.7 Both arms were not run under equivalent conditions

The arms share a working directory, CratonVM always runs first, and whatever it
leaves behind is the oracle's input. On H2 this is not theoretical: a
carried-over `data/` store made `TestBackup` DIVERGE where all three arms are
green run alone (`P4A-H2-DIVERGENCES-20260812.md` §3c). New
`CORPUS_CLEAN_PATHS` support removes corpus-declared scratch state **before
each arm**, refusing absolute paths, `..` and globs. Exercised. No corpus
declares it yet — that is NOMINATION 1, and the standing hazard is written up in
**`C8-H2-TESTBACKUP-SHARED-WORKDIR-20260812.md`**.

Also fixed here: the workdir is validated once, loudly, before the loop. A
failing `cd` inside the arm subshell used to make the arm exit 1 without running
anything, which classified as `NOSTART` — i.e. as a classpath defect.

### 2.8 Identical failure on both arms scored AGREE

Documented in `corpora.d/commons-math.sh` and never fixed on the driver side:
the JUnit platform launcher throws `JUnitException` during discovery on **both**
arms, the keys match, and the row scores `AGREE` with zero tests run. The
mirror case scored `DIVERGE` against an oracle that had itself failed
(`AccurateMathTest`: HotSpot discovered nothing, CratonVM ran 70).

`oracle_vacuous` now returns vacuous for two more shapes, closing both
directions (this is `P4A-CORPORA-20260812.md` NOMINATION 1, applied):

* a junit-kind oracle that threw with **no `SBRUNNER_RESULT` at all** — it died
  during discovery;
* a **linkage-family** throw escaping to the wrapper on the oracle
  (`NoClassDefFoundError`, `ClassNotFoundException`, `NoSuchMethodError`,
  `NoSuchFieldError`, `IncompatibleClassChangeError`,
  `UnsupportedClassVersionError`, `ExceptionInInitializerError`, `JUnitException`,
  `LinkageError`) — the fixture is broken on the reference side, and an
  identical failure on both arms is not agreement.

### 2.9 Small ones, same species

* **CRLF in a class list.** This repo checks out with `core.autocrlf=true`. A
  `\r` inside a class name produces a `NOSTART` row on both arms that reads as a
  launcher defect. The list is now stripped and each entry is validated as an
  FQCN, with a mangled entry refused up front instead of becoming a row.
* **`discover --limit N`** piped into `head` under `set -o pipefail`: the
  producer dies of SIGPIPE and a **successful** discovery exits non-zero. Now
  buffered.
* **Notes containing a tab or newline** would silently add columns or rows to
  the TSV. Sanitised.

---

## 3. The self-check (`bash run-corpus.sh selfcheck`)

The precedent is `scripts/check-no-diag-prints.sh`: a BLOCKING gate whose search
ended in `|| true`, so "clean tree" and "the search failed" were the same
output, fixed by a sentinel pattern that must always match. The corpus driver
has the same exposure somewhere worse — a comparison that cannot go red reports
a green corpus — so it now carries its own control. **39 assertions, all
passing**, in two halves:

* **unit** (33): `classify_arm` over every state; `arm_key`; `oracle_vacuous`;
  `decode_status`; `adjudicate`. Two of them are the sentinels — *a real count
  difference must DIVERGE*, and *an empty key must be HARNESS-ERROR, never
  AGREE*.
* **end-to-end** (6): runs the real `cmd_run` twice against a throwaway corpus
  with **real HotSpot as the oracle** and a stub standing in for
  `cratonvm.exe`. The stub forwards to real `java` after dropping the CratonVM
  flags, so the "VM" arm genuinely executes the workload: once identically (must
  exit 0 with `AGREE=1`) and once with one injected argument that makes the
  workload throw (**must exit 1 with `DIVERGE=1`**). Plus the empty-workload and
  bad-`--timeout` refusals.

The end-to-end half exists because of the recorded failure mode where *a
mutation test was written for a `ripgrep` code path on a host with no ripgrep
installed, and so tested nothing.* This one drives the same `cmd_run` that
produces every corpus result, on this host, with this JDK; the transcript shows
the rows and the summary lines it asserted on. Adjudication was extracted into
one `adjudicate` function precisely so the self-check exercises the production
code rather than a paraphrase of it.

`--unit-only` skips the ~40 s end-to-end half and says, in its own output, that
the unit half alone **cannot** prove the driver can go red.

---

## 4. What is invalidated, and what must be re-taken

### 4.1 Invalidated: every `DIVERGE` count from a `junit`-kind corpus

Any claim of the form "corpus X reported N divergences under `--jdk-only`" that
was taken before this fix is **not usable**, for `bc-java`, `commons-math`,
`spring-framework`, and any future junit-kind corpus. Specifically:

* `bc-java`: the reported `AGREE=0 DIVERGE=9` / `DIVERGE=13` runs. The correct
  reading of the 13-row run is **7 fake, 6 genuine** (all six carrying `cv_rc=1`
  against `hs_rc=0`, i.e. they differ in content as well as in the marker).
* `commons-math`: `DIVERGE=6` → 2 genuine; `DIVERGE=4` → **0 genuine**.
* `spring-framework`: `DIVERGE=27` → **1 genuine**; `DIVERGE=3` → 0 genuine.
* Any statement of the form "all but N `DIVERGE` rows are the shutdown-hook
  artefact" is now measurable rather than estimated: the numbers are §2.1.

**These do not need the VM to re-take.** The per-row `.cv.log`/`.hs.log` files
are on disk; re-adjudicating them with the fixed key is what produced §2.1. A
consumer wanting corrected counts should re-adjudicate the stored logs rather
than spend VM time.

### 4.2 Invalidated: every `AGREE` on a row where the oracle failed identically

`AGREE` rows whose logs show a discovery-time or linkage-family `CORPUS-THROW`
on the HotSpot side were "we learned nothing" printed as "we verified it". The
known instances are `commons-math`'s `JdkMathTest` first run and the bc-java
rows from the same misaligned-JUnit period (`P4A-CORPORA-20260812.md` §1). Under
the fixed `oracle_vacuous` they become `ORACLE-VACUOUS`. Re-adjudicable from
stored logs.

### 4.3 Needs re-taking with the VM: every `CV-TIMEOUT` row

`CV-TIMEOUT` rows cannot be reclassified from stored logs alone — the
STALLED/BUSY split needs the kill time, which was not recorded. The stored rows
are: 3 in `bc-java-…-213333`, 1 in `bc-java-…-220129`, 2+1 in the `h2` runs, 2+1
in the `spring-framework` runs. `P4A-H2-DIVERGENCES-20260812.md` §3a/§3b already
declines to attribute two of them, for the right reason. **They stay
unattributed**; the taxonomy record says what a re-run must record.

One row can be partly read today: `bc-java`'s
`crypto.hash2curve.test.AllTests`, killed at 1,509,793 ms against an oracle that
finished in 112,531 ms, has a `.cv.log` whose last write was **552 s before the
kill**. Under the new classifier that is `TIMEOUT-STALLED`, not slowness. Three
CV timeouts clustered on arithmetic-heavy suites; two of them were only ever
tried at the 420 s cap and are **not** separated from the wall. The harness now
says so on the row instead of forcing a verdict.

### 4.4 Not invalidated

* The `h2` runs' `DIVERGE` rows (main-kind: nothing flips) — §2.1's control.
* `ORACLE-VACUOUS` and `ORACLE-UNUSABLE` rows: they were unadjudicated before
  and still are.
* Everything `P4A-CORPORA-20260812.md` §A establishes about
  `SimpleTimeZone(rawOffset, ID)`: it was isolated to four lines of Java outside
  the corpus driver entirely.

---

## 5. Which fixes are exercised, and which are not

**This lane could not run `cratonvm.exe`.** The split matters.

### Exercised (measured on this host, this session)

| fix | how |
|---|---|
| hook-END key fix | 39 self-check assertions **and** re-adjudication of 75 stored DIVERGE rows |
| empty workload list | reproduced exit 0 before, exit 2 after |
| non-numeric `--timeout` | exit 3, through `cmd_run` |
| POSIX `--out` | reproduced `ORACLE-UNUSABLE` before, correct row after |
| preflight (success path) | oracle version line now in every TSV header |
| `SIGNAL` | stub `kill -SEGV $$` → `CV-SIGNAL`, `signal=11(SIGSEGV)` |
| `TIMEOUT-STALLED` | stub sleeps → *"after 41s with NO output: SILENT AT THE WALL"* |
| `TIMEOUT-BUSY` | stub prints every 2 s → *"last write 2s before the kill"*, and the row refuses to assert hung-vs-slow |
| `LAUNCH-FAILED` → `HARNESS-ERROR` | stub with a bad shebang → exit 2, not counted against the VM |
| CRLF class list | `class\r\n` list runs the class |
| FQCN validation | `org/h2/Bad Name` refused, exit 2 |
| `CORPUS_CLEAN_PATHS` | planted `data/` removed; `../escape` and `/etc` refused |
| `discover --limit` | exits 0 |
| self-check, both halves | 39/39 pass, transcript in this session |
| `bash -n` | clean, run as the last action of this lane |

### NOT exercised — unverified by construction

* **NTSTATUS decoding against a real Windows access violation.** `decode_status
  3221225477` is unit-tested; no real `0xC0000005` was produced, because that
  needs the VM. The number and the mapping are from this repo's existing
  records, not from an observation made this session.
* **The preflight FAILURE branch.** Its cause (`--out` as a POSIX path) is now
  fixed, so it could not be triggered without re-breaking the fix.
* **`oracle_vacuous`'s two new rules on a real corpus.** Unit-tested on
  synthetic logs; not re-run against `commons-math` with a misaligned JUnit
  classpath, which needs a corpus run.
* **`CRASH` from log text via `cmd_run`.** Unit-tested; the stubs produce
  status-borne death, not rust panics.
* **The empty-key `HARNESS-ERROR` guard.** Unit-tested only; it cannot be
  reached without deliberately breaking `arm_key`.
* **The whole driver against a real `cratonvm.exe`.** Nothing in this record was
  measured with the VM. The first real corpus run after this change should be
  read with that in mind — and should begin with `selfcheck`.

---

## 6. NOMINATIONS (files this lane does not own)

### NOMINATION 1 — `regression-suite/corpus/corpora.d/h2.sh`

H2 writes its databases into the working directory and the arms share it.
Declare the scratch state so the driver removes it before each arm.

Replace literally:

```sh
# H2 writes database files relative to the working directory, so every arm
# must run from the same place or the two arms are not running the same
# workload. Both arms get this directory.
corpus_workdir() { echo "$1"; }
```

with:

```sh
# H2 writes database files relative to the working directory, so every arm
# must run from the same place or the two arms are not running the same
# workload. Both arms get this directory.
corpus_workdir() { echo "$1"; }

# ...and the same directory is why the arms must not INHERIT each other's
# files. The CratonVM arm runs first; whatever it leaves in `data/` is the
# oracle's input. A carried-over store made `TestBackup` fail with
# `MVStoreException: Chunk 2 not found` while all three arms are green run
# alone (docs/known-issues/jdk-only/P4A-H2-DIVERGENCES-20260812.md §3c, and
# C8-H2-TESTBACKUP-SHARED-WORKDIR-20260812.md). run-corpus.sh removes these
# paths, relative to the workdir, before EACH arm.
CORPUS_CLEAN_PATHS="data"
```

### NOMINATION 2 — `regression-suite/corpus/README.md`

Its two tables now under-describe the driver. Replace literally:

```
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
```

with:

```
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

## The driver proves it can go red

`bash run-corpus.sh selfcheck` is a standing positive control: 39 assertions
over the real classification/comparison/adjudication code, including an
end-to-end pair that runs `cmd_run` itself against HotSpot with a stub VM —
once agreeing (must exit 0) and once diverging (**must exit 1**). Run it before
believing a red corpus, and after editing this driver. Precedent and rationale:
`scripts/check-no-diag-prints.sh`, and
`docs/known-issues/jdk-only/C8-CORPUS-HARNESS-DEFECTS-20260812.md`.
```

### NOMINATION 3 — `regression-suite/corpus/corpora.d/commons-math.sh`

Its comment still says the driver-side half of the fix is outstanding. Replace
literally:

```sh
# That is worse than a broken run. The failure is IDENTICAL on both arms, so
# the markers match and run-corpus.sh scores the class **AGREE** -- measured
# here on 2026-08-12, on this corpus and on bc-java. `oracle_vacuous` does not
# catch it: it inspects the SBRUNNER_RESULT line, and this failure never gets
# far enough to print one. So the guard against "we learned nothing rendering
# as we verified it" has a hole exactly here, and the fix belongs in BOTH
# places -- see the NOMINATION in docs/known-issues/jdk-only/P4A-CORPORA-20260812.md
# for the run-corpus.sh half, which this lane may not edit.
```

with:

```sh
# That is worse than a broken run. The failure is IDENTICAL on both arms, so
# the markers match and run-corpus.sh scored the class **AGREE** -- measured
# here on 2026-08-12, on this corpus and on bc-java.
#
# The run-corpus.sh half of the fix LANDED on 2026-08-12 (lane C8):
# `oracle_vacuous` now returns vacuous for a junit-kind oracle that threw with
# no SBRUNNER_RESULT at all (it died during DISCOVERY) and for a
# linkage-family throw escaping to the wrapper. Either shape is reported as
# ORACLE-VACUOUS -- unadjudicated -- instead of AGREE. See
# docs/known-issues/jdk-only/C8-CORPUS-HARNESS-DEFECTS-20260812.md §2.8.
# The classpath half below is still what stops the failure happening at all.
```

### NOMINATION 4 — `docs/known-issues/jdk-only/P4A-CORPORA-20260812.md`

Mark both driver nominations resolved, and record that one of them was
insufficient as written. Replace literally:

```
### NOMINATION 2 (`regression-suite/corpus/run-corpus.sh`)
```

with:

```
### NOMINATION 2 (`regression-suite/corpus/run-corpus.sh`) — APPLIED 2026-08-12, IN A CORRECTED FORM

**The patch below is insufficient and must not be applied as written.** Run
against the real logs it still diverges: stripping the ` completed=` qualifier
leaves a bare `CORPUS-END SbRunner` line present on HotSpot and absent on
CratonVM, so the row is still DIVERGE. It also erases the `true` vs `exit`
distinction, which is the only signal separating a bytecode-sourced marker from
a hook-sourced one. What landed instead deletes ONLY the hook-sourced line
(`sed '/^CORPUS-END .* completed=exit$/d'`), keeps `completed=true`, compares
exit statuses when the keys match, and reports the hook asymmetry as a note on
the row. Measured effect: 62 of 75 stored DIVERGE rows are the artefact. See
C8-CORPUS-HARNESS-DEFECTS-20260812.md §2.1.
```

and replace literally:

```
### NOMINATION 1 (`regression-suite/corpus/run-corpus.sh`)
```

with:

```
### NOMINATION 1 (`regression-suite/corpus/run-corpus.sh`) — APPLIED 2026-08-12
```

### NOMINATION 5 — `regression-suite/corpus/CorpusMain.java` (optional)

Its contract block is correct and needs no code change. One line would stop the
next reader re-deriving §2.1. After the line

```
 *   CORPUS-END &lt;fqcn&gt; completed=exit  target called System.exit; seen via shutdown hook
```

add:

```
 *
 * NOTE: run-corpus.sh EXCLUDES the `completed=exit` line from its comparison
 * key. It is the one marker here that is not printed by the workload's own
 * bytecode, CratonVM runs no shutdown hooks, and keying on it manufactured 62
 * false divergences across nine corpus runs (see
 * docs/known-issues/jdk-only/C8-CORPUS-HARNESS-DEFECTS-20260812.md). The
 * normal-path `completed=true` marker IS keyed on and must stay on the
 * non-hook path.
```

---

## 7. The lesson, stated for the next lane

Eight distinct instances of "the instrument reported its own defect as a
finding" were documented in this session; three came from this one file, and
this pass found six more in it. The recurring shape is not carelessness — every
one of these was written by somebody who had just been burned by the previous
one, and the comments in this file prove it. The shape is that **a harness
defect is indistinguishable from its subject's defect by construction**, unless
the harness carries a control that can only pass when the harness works.

So: an oracle that is sick must not score green; a gate that measures nothing
must not read as good news; a verdict that folds three suspects into one word
will be quoted as whichever suspect the reader already believed in. And a
harness that cannot demonstrate it is able to go red has not earned any of its
green.
