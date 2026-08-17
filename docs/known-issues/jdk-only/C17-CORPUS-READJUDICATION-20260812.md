# C17 — re-adjudicating every stored corpus run: 62 of 75 divergences were the harness, and four "agreements" were the VM running nothing

**Date:** 2026-08-12 **Lane:** C17 **Files owned:**
`regression-suite/corpus/corpora.d/*.sh`, `regression-suite/corpus/README.md`,
`docs/known-issues/jdk-only/P4A-CORPORA-20260812.md`, this record.

**This lane never built or ran CratonVM.** Every number below comes from
`java`/`bash` over the **stored logs already on disk** under
`regression-suite/corpus/out/`, replayed through the *production* adjudication
code of the repaired `run-corpus.sh`. `run-corpus.sh` was not edited.

---

## 0. Headline

**All 163 stored corpus rows across all 18 stored runs were re-adjudicable
without the VM. 79 rows changed; 84 did not.**

| | rows |
|---|---|
| `DIVERGE` → `AGREE`, cause = hook-sourced `CORPUS-END` in the key | **62** |
| `DIVERGE` → `ORACLE-VACUOUS`, cause = the new `oracle_vacuous` rules | 2 |
| `AGREE` → `ORACLE-VACUOUS`, same cause | 2 |
| `ORACLE-VACUOUS` → `HARNESS-ERROR`, cause = a real `fork` failure (rc=126) | 1 |
| `CV-TIMEOUT` → `CV-TIMEOUT-UNKNOWN` — a **relabel, not a resolution** | 12 |
| unchanged | 84 |

C8's headline reproduces **exactly**: 62 of 75, row for row, and every one of
the 62 is individually attributable — for each, the HotSpot log carries
`CORPUS-END … completed=exit` and the CratonVM log does not.

**Two findings C8 did not have, both from rows it did not re-check:**

1. **Four bc-java rows that `P4A-CORPORA` §5 recorded as "agrees, failed=0 on
   both arms" are rows on which CratonVM ran ZERO tests.** They are among the
   6 surviving divergences, not among the agreements. §3.
2. **A bc-java row was a `fork`/`exec` failure on this host** — `timeout: failed
   to run command '…cratonvm.exe': Resource temporarily unavailable`, `rc=126`.
   The old harness called it `NOSTART`, i.e. a corpus classpath problem. It is
   now `HARNESS-ERROR`, and it is the first observation of C8 §2.4 in **real
   stored evidence** rather than against a stub. §4.

And the control held: **h2 moved 0 of 3 divergences and 0 of 22 agreements.**

---

## 1. Method — how a stored run is re-adjudicated without the VM

`run-corpus.sh` is, by construction, entirely function definitions followed by a
single `main "$@"` on its last line. Deleting that one line yields a sourceable
library of the exact functions `cmd_run` uses. The replay therefore calls
`classify_arm`, `arm_key`, `oracle_vacuous`, `has_hook_end`, `decode_status` and
`adjudicate` **themselves** — not a paraphrase of them, which is the failure
mode this whole session keeps hitting.

Per row, the inputs are taken from the stored artefacts: `cv_rc`/`hs_rc`,
`cv_ms`/`hs_ms` and the old verdict from `results.tsv`; the cap from the TSV's
`# timeout=<N>s` header; `CORPUS_KIND` from the corpus name; and the arms'
`.cv.log` / `.hs.log` verbatim.

Two deliberate refusals, because both would have manufactured answers:

* **`rc=124` rows are given a NON-NUMERIC end-epoch.** `classify_arm` splits the
  wall by seconds-of-silence, which needs the kill time, and no run before
  2026-08-12 recorded it. A non-numeric value makes `log_silence_secs` return
  `-1`, so the row becomes `TIMEOUT-UNKNOWN`. Passing `0` instead — the obvious
  thing — would have clamped the silence to 0 s and reported every stored
  timeout as `TIMEOUT-BUSY`, i.e. *"still writing at the wall"*, a fabricated
  claim about 12 rows.
* **`hs_state=SKIPPED` (`--no-oracle`) is preserved, not re-derived.** The first
  pass of this replay re-classified those 4 rows from an absent oracle log and
  turned `UNADJUDICATED` into `ORACLE-UNUSABLE` — a verdict change produced by
  the *replay*, not by the driver fix. Caught, fixed, re-run; the 4 rows are
  unchanged. It is recorded here because it is the same species as everything
  else in this file: **the instrument was about to report its own defect as a
  finding.**

Script: `scratchpad/c17/readjudicate.sh`; row-level output:
`scratchpad/c17/readj-final.tsv` (164 lines, 1 header + 163 rows).

---

## 2. The corrected tables

### 2.1 Per corpus — BEFORE (as the runs reported) and AFTER

| corpus | rows | verdict | BEFORE | AFTER |
|---|---|---|---|---|
| **bc-java** | 80 | `AGREE` | 1 | **25** |
| | | `DIVERGE` | 31 | **6** |
| | | `CV-TIMEOUT*` | 4 | 4 (all `-UNKNOWN`) |
| | | `ORACLE-VACUOUS` | 42 | 42 |
| | | `ORACLE-UNUSABLE` | 2 | 2 |
| | | `HARNESS-ERROR` | 0 | **1** |
| **commons-math** | 12 | `AGREE` | 1 | **8** |
| | | `DIVERGE` | 10 | **0** |
| | | `ORACLE-VACUOUS` | 1 | **4** |
| **spring-framework** | 41 | `AGREE` | 0 | **29** |
| | | `DIVERGE` | 31 | **2** |
| | | `CV-TIMEOUT*` | 3 | 3 (all `-UNKNOWN`) |
| | | `ORACLE-VACUOUS` | 2 | 2 |
| | | `ORACLE-UNUSABLE` | 1 | 1 |
| | | `UNADJUDICATED` | 4 | 4 |
| **h2** *(control)* | 30 | `AGREE` | 22 | **22** |
| | | `DIVERGE` | 3 | **3** |
| | | `CV-TIMEOUT*` | 5 | 5 (all `-UNKNOWN`) |

**h2 is the control and it held.** Not one h2 verdict moved. h2 is `main`-kind:
its workloads return normally, `CORPUS-END … completed=true` is printed by
bytecode on both arms, and the dropped line is never present on either. The fix
touched exactly the rows it should. Had h2 moved, the procedure would have been
wrong — that check is why it was run over all 30 h2 rows and not just the 3
divergent ones.

### 2.2 Per run

| run | BEFORE | AFTER |
|---|---|---|
| `bc-java-…-204042` (1) | `AGREE=1` | `ORACLE-VACUOUS=1` |
| `bc-java-…-205739` (30) | `DIVERGE=9 ORACLE-VACUOUS=21` | `AGREE=9 ORACLE-VACUOUS=20 HARNESS-ERROR=1` |
| `bc-java-…-210742` (30) | `DIVERGE=9 ORACLE-VACUOUS=21` | `AGREE=9 ORACLE-VACUOUS=21` |
| `bc-java-…-213333` (18) | `DIVERGE=13 CV-TIMEOUT=3 ORACLE-UNUSABLE=2` | `AGREE=7 DIVERGE=6 CV-TIMEOUT-UNKNOWN=3 ORACLE-UNUSABLE=2` |
| `bc-java-…-220129` (1) | `CV-TIMEOUT=1` | `CV-TIMEOUT-UNKNOWN=1` |
| `commons-math-…-204005` (1) | `AGREE=1` | `ORACLE-VACUOUS=1` |
| `commons-math-…-204748` (1) | `ORACLE-VACUOUS=1` | `ORACLE-VACUOUS=1` |
| `commons-math-…-204935` (6) | `DIVERGE=6` | `AGREE=4 ORACLE-VACUOUS=2` |
| `commons-math-real-jdk-…-205438` (4) | `DIVERGE=4` | `AGREE=4` |
| `h2-…-195200` (1) | `AGREE=1` | `AGREE=1` |
| `h2-…-195247` (14) | `AGREE=10 DIVERGE=2 CV-TIMEOUT=2` | `AGREE=10 DIVERGE=2 CV-TIMEOUT-UNKNOWN=2` |
| `h2-real-jdk-…-171131` (1) | `CV-TIMEOUT=1` | `CV-TIMEOUT-UNKNOWN=1` |
| `h2-real-jdk-…-203313` (14) | `AGREE=11 DIVERGE=1 CV-TIMEOUT=2` | `AGREE=11 DIVERGE=1 CV-TIMEOUT-UNKNOWN=2` |
| `spring-framework-…-204553` (32) | `DIVERGE=27 CV-TIMEOUT=2 ORACLE-VACUOUS=2 ORACLE-UNUSABLE=1` | `AGREE=26 DIVERGE=1 CV-TIMEOUT-UNKNOWN=2 ORACLE-VACUOUS=2 ORACLE-UNUSABLE=1` |
| `spring-framework-…-211153` (4) | `DIVERGE=3 CV-TIMEOUT=1` | `AGREE=3 CV-TIMEOUT-UNKNOWN=1` |
| `spring-framework-…-215547` (1) | `DIVERGE=1` | `DIVERGE=1` |
| `spring-framework-real-jdk-…-213155` (2) | `UNADJUDICATED=2` | `UNADJUDICATED=2` |
| `spring-framework-real-jdk-…-213603` (2) | `UNADJUDICATED=2` | `UNADJUDICATED=2` |

### 2.3 Grand totals, and the denominator

| verdict | BEFORE | AFTER |
|---|---|---|
| `AGREE` | 24 | **84** |
| `DIVERGE` | 75 | **11** |
| `CV-TIMEOUT` / `CV-TIMEOUT-UNKNOWN` | 12 | 12 |
| `ORACLE-VACUOUS` | 45 | 48 |
| `ORACLE-UNUSABLE` | 3 | 3 |
| `UNADJUDICATED` (`--no-oracle`) | 4 | 4 |
| `HARNESS-ERROR` | 0 | 1 |
| **total** | **163** | **163** |

**Say the denominator.** Of 163 stored rows, **95 were adjudicated at all**
(84 `AGREE` + 11 `DIVERGE`). The other 68 are `ORACLE-VACUOUS` (48),
`CV-TIMEOUT-UNKNOWN` (12), `ORACLE-UNUSABLE` (3), `UNADJUDICATED` (4) and
`HARNESS-ERROR` (1) — **nothing was learned about the VM from any of them.**
"84 of 95 agree" is the claim. "84 agree, 11 diverge, 68 never produced
evidence" is the same claim said properly, and this project has published the
first shape as though it covered the whole corpus.

### 2.4 And 9 of the 84 agreements are agreement with a RED oracle

`oracle_vacuous` refuses an oracle that discovered nothing or aborted
everything. It does **not** refuse an oracle that ran and *failed*. Nine `AGREE`
rows are of that shape:

| corpus | class | HotSpot |
|---|---|---|
| commons-math ×2 runs | `legacy.core.MathArraysTest` | `tests=44 failed=6` |
| commons-math ×2 runs | `legacy.core.jdkmath.AccurateMathStrictComparisonTest` | `tests=1 failed=1` |
| spring-framework | `aop.aspectj.AbstractAspectJAdviceTests` | `tests=6 failed=6` |
| spring-framework | `aop.aspectj.AfterReturningAdviceBindingTests` | `tests=12 failed=12` |
| spring-framework | `beans.factory.config.YamlPropertiesFactoryBeanTests` | `tests=17 failed=17` |
| spring-framework | `aop.aspectj.autoproxy.AspectJAutoProxyCreatorAndLazyInitTargetSourceTests` | `tests=1 failed=1` |
| spring-framework | `scheduling.quartz.QuartzSchedulerLifecycleTests` | `tests=2 failed=2` |

Five of them are **failed == tests**: the whole class fails on HotSpot 25 on this
host. Agreeing with a reference run that failed every test is not evidence the
VM works; it is evidence the two arms tallied the same broken fixture the same
way. **The defensible figure is 75 agreements against a green oracle** — 25
bc-java, 4 commons-math, 24 spring-framework, 22 h2 — **and 9 against a red
one.** Zero surviving `AGREE` rows have an oracle-side `CORPUS-THROW`; that
class of false green is fully closed by the new `oracle_vacuous`.

`oracle_vacuous` treating `aborted == tests` as vacuous while ignoring
`failed == tests` is asymmetric. It is deliberate (a failure is an answer, an
abort is not), but the row should carry the fact. **NOMINATION C — see §7.**

---

## 3. The four bc-java rows `P4A-CORPORA` §5 got backwards

`P4A-CORPORA-20260812.md` §5 tabulates the `bc-java-…-213333` run and records
these four suites as **"agrees"**, with `failed=0` in *both* the HotSpot and the
CratonVM column:

```
util.encoders.test.AllTests   15  failed=0  failed=0  agrees
util.utiltest.AllTests        13  failed=0  failed=0  agrees
util.io.pem.test.AllTests      7  failed=0  failed=0  agrees
pqc.math.ntru.test.AllTests   22  failed=0  failed=0  agrees
```

The stored logs of the only run in which those suites exist say otherwise. For
all four, verbatim:

```
--- HotSpot ---                                  --- CratonVM ---
CORPUS-START SbRunner                            CORPUS-START SbRunner
SBRUNNER_RESULT tests=15 failed=0 …              CORPUS-THROW SbRunner org.junit.platform.commons.JUnitException
CORPUS-END SbRunner completed=exit
```

**CratonVM ran zero tests.** There is no CratonVM `failed=0` to report, and no
stored log anywhere supports the number that was published. These are the same
four rows C8 counted as "genuine" without saying what they were — C8's
`cv_rc=1 vs hs_rc=0` observation is correct and is exactly this.

The thrown message is the diagnosis and it is not a fixture problem:

```
org.junit.platform.commons.JUnitException:
  Cannot create Launcher for multiple engines with the same ID 'junit-jupiter'.
    at org.junit.platform.launcher.core.EngineIdValidator.validate(EngineIdValidator.java:42)
    at org.junit.platform.launcher.core.LauncherFactory.createDefaultLauncher(LauncherFactory.java:141)
    …
    at SbRunner.main(SbRunner.java:80)
```

Both arms of a row share one `cp.args` file, so the classpath is byte-identical.
HotSpot's `ServiceLoader` found the `junit-jupiter` `TestEngine` provider once;
CratonVM's found it **more than once**. That is a VM-side resource/service
enumeration defect, not a version skew.

Three things bound the claim:

* **It is intermittent.** Nine other classes in the *same run*, same classpath,
  same binary, did not hit it. One process per class, so it varies across
  processes — the shape of an unordered container or a
  duplicate-resource-URL path, not a deterministic mis-resolution.
* **A lead, not a proof.** The same `.cv.log` carries
  `refusing to fabricate … class="java/util/Enumeration$Impl"
  requested_by="native-builtins\src\classloader.rs:6074"`, i.e. the
  `ClassLoader.getResources` enumeration path, which is precisely what
  `ServiceLoader` walks. That is where to look first; it is not evidence yet.
* **It appears in exactly 4 of 163 stored `.cv.log`s and 0 of 163 `.hs.log`s.**

**This needs VM time to confirm and is listed in §6.**

### The corrected reading of `bc-java-…-213333`

| | raw column said | `P4A` §5 said | measured here |
|---|---|---|---|
| `AGREE` | 0 | 11 | **7** |
| `DIVERGE` | 13 | 2 | **6** — 2 assertion-level (`asn1` §A, `i18n` §B) + **4 CratonVM launcher throws** |
| `CV-TIMEOUT*` | 3 | 3 | 3, all `-UNKNOWN` |
| `ORACLE-UNUSABLE` | 2 | 2 | 2 |

`P4A`'s "11 of 13 agree" is wrong in **both** directions: the raw `AGREE=0` was
too pessimistic, and the correction over-shot by assuming every non-`asn1`,
non-`i18n` divergence was the marker artefact. Seven were. Four were the VM
running nothing. **A spot-check that finds the first two rows fake and stops is
exactly the failure C8 predicted this harness would cause.**

---

## 4. A `fork` failure that had been reading as a classpath defect

`bc-java-…-205739`, `org.bouncycastle.pqc.crypto.faest.AesWitnessExtensionTest`.
The whole CratonVM log is one line:

```
timeout: failed to run command '/c/craton/jdkonly-wave2-target/release/cratonvm.exe': Resource temporarily unavailable
```

`cv_rc=126`. The old classifier had no bucket for it, so it fell to `NOSTART`,
whose note sends the reader to *the corpus's classpath*. The row escaped being
scored against the VM only by luck — the oracle happened to be vacuous, so it
was reported as `ORACLE-VACUOUS`, which is the wrong reason for the right
outcome. Under the repaired classifier it is `LAUNCH-FAILED` → `HARNESS-ERROR`,
exit 2, never scored against the VM.

`EAGAIN` from `fork`/`exec` under Git Bash on this host means the machine was
out of process slots at that instant. It is a **load** artefact, and this
session's other lanes are running concurrently on the same box. C8 could only
exercise this path with a bad-shebang stub; this is the same defect caught in
real stored evidence.

---

## 5. The 11 surviving divergences

| corpus / run | class | shape |
|---|---|---|
| bc-java 213333 | `asn1.test.AllTests` | `tests=20 failed=0` vs `failed=1` — `SimpleTimeZone(rawOffset,ID)`, `P4A` §A |
| bc-java 213333 | `i18n.test.AllTests` | `tests=5 failed=0` vs `failed=1` — German pattern / zone display name, `P4A` §B |
| bc-java 213333 | `pqc.math.ntru.test.AllTests` | HS `tests=22 failed=0`; CV `CORPUS-THROW JUnitException` — §3 |
| bc-java 213333 | `util.encoders.test.AllTests` | HS `tests=15 failed=0`; CV throws — §3 |
| bc-java 213333 | `util.io.pem.test.AllTests` | HS `tests=7 failed=0`; CV throws — §3 |
| bc-java 213333 | `util.utiltest.AllTests` | HS `tests=13 failed=0`; CV throws — §3 |
| spring 204553 | `core.test.tools.CompiledTests` | HS `tests=14 failed=9`; CV `failed=14` — 5 extra failures **against an already-red oracle** |
| spring 215547 | `util.ObjectUtilsTests` | HS `tests=140 failed=0`; CV `failed=2` — clean, green oracle, 2 real failures |
| h2 195247 | `db.TestAlterSchemaRename` | HS `completed=true`; CV `CORPUS-THROW JdbcSQLNonTransientException` |
| h2 195247 | `db.TestCases` | HS `completed=true`; CV `CORPUS-THROW JdbcSQLNonTransientException` |
| h2 203313 (`--real-jdk`) | `db.TestBackup` | HS `completed=true`; CV `CORPUS-THROW JdbcSQLNonTransientException` |

**`TestBackup` must not be read as a VM finding.** That run had no
`CORPUS_CLEAN_PATHS` (the declaration landed in this lane, §7 NOMINATION 1), so
the HotSpot arm started in a directory the CratonVM arm had just written. See
`C8-H2-TESTBACKUP-SHARED-WORKDIR-20260812.md`. It is `DIVERGE` on the corrected
table because that is what the stored evidence says; it is **not** attributable
until it is re-run with the cleaning in place.

`spring 215547 ObjectUtilsTests` is the highest-value row here: a green oracle,
140 tests, byte-identical counts except 2 CratonVM failures, and a corpus that
otherwise agrees 24/24 against a green oracle.

---

## 6. What still needs VM time — and nothing else does

Exactly two things. Everything else in this record is settled from disk.

1. **The 12 `CV-TIMEOUT-UNKNOWN` rows. UNRESOLVED — do not guess.**
   3 `bc-java` (`crypto.hash2curve`, `math.ec.test`, `pqc.crypto.lms` in
   `…-213333`), 1 `bc-java` (`crypto.hash2curve` at 1500 s in `…-220129`),
   2+1 `h2`, 2+1 `spring-framework`, 2+2 `spring-framework --no-oracle`. The
   stalled/busy split needs the kill time; no run before 2026-08-12 recorded it,
   and the repaired driver now does. **A re-run of just these classes, under the
   current driver, resolves all 12 and needs nothing else.**
   `crypto.hash2curve` is already argued to be `TIMEOUT-STALLED` from its log's
   last-write time (C8 §4.3); the other 11 are not.
2. **The duplicate-`junit-jupiter`-engine throw (§3).** Re-run
   `org.bouncycastle.util.utiltest.AllTests`,
   `org.bouncycastle.util.encoders.test.AllTests`,
   `org.bouncycastle.util.io.pem.test.AllTests` and
   `org.bouncycastle.pqc.math.ntru.test.AllTests` under `--jdk-only`. It is
   intermittent, so a single green re-run does **not** clear it — the useful
   product is a small `ServiceLoader`/`getResources` probe run N times, not a
   corpus row.

Not needed: re-running anything to obtain corrected `AGREE`/`DIVERGE` counts.
All of those are in §2.

---

## 7. NOMINATIONS APPLIED, and three new ones

### Applied by this lane (files it owns)

| # | file | what |
|---|---|---|
| 1 | `corpora.d/h2.sh` | `CORPUS_CLEAN_PATHS="data"` added, per C8 NOMINATION 1, **plus** a warning that `ext` must never be added — `ext/` is also in H2's own `.gitignore` and is where `corpus_classpath` gets every dependency jar, so declaring it would delete the classpath before the first arm and produce ~200 identical `NoClassDefFoundError`s. `temp` is named as an unmeasured candidate and deliberately not declared. |
| 2 | `corpora.d/commons-math.sh` | comment updated: the driver-side half of the `oracle_vacuous` fix has landed; the hole it describes is closed. |
| 3 | `corpus/README.md` | four-state table → eight states; new verdict table with buckets; the denominator rule; a section on why the hook-`END` line is excluded from the key **and why stripping `completed=` is the wrong fix**; the `selfcheck` section. |
| 4 | `P4A-CORPORA-20260812.md` | NOMINATION 1 marked APPLIED; NOMINATION 2 marked APPLIED-IN-A-CORRECTED-FORM with a block quote **above** the patch explaining that the patch as written must not be used, so a reader who skims to the code cannot miss it. Historical ratios corrected in §R. |

### NOMINATION A — `regression-suite/corpus/CorpusMain.java` (C8 §6's fifth, endorsed)

Correct as written and worth taking. After the line

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

### NOMINATION B — `C8-CORPUS-HARNESS-DEFECTS-20260812.md`, the self-check count

The record says **39 assertions (33 unit + 6 end-to-end)** in §0/§3/§5, and
`README.md`'s proposed text repeated it. Measured on this host, this session:
**38 pass, 0 fail, exit 0 — 32 unit + 6 end-to-end.** The source has 42
`sc_assert` call sites, of which 8 are four two-armed `case` pairs that print
one line each, giving 32 distinct unit assertions. Nothing is skipped and
nothing is broken; the published count is one too high. `README.md` was written
with **38** rather than propagating 39. Replace `39 assertions` / `33 unit`
in that record with `38` / `32`.

*(This is a documentation defect, not an instrument defect. It is nominated
because a future reader who runs `selfcheck`, counts 38, and expects 39 will
correctly suspect a silently skipped assertion — and will burn a lane proving
there is none.)*

### NOMINATION C — `regression-suite/corpus/run-corpus.sh`, an oracle that failed everything

`oracle_vacuous` refuses `aborted == tests` and admits `failed == tests`. The
asymmetry is defensible — a failure is an answer — but 5 of the 84 surviving
`AGREE` rows are agreement with a class that failed **every** test on HotSpot
(§2.4), and nothing on the row says so. That is the "we learned nothing rendered
as we verified it" shape arriving one step further down.

Not offered as a literal patch, because the right treatment is a judgement call
for the file's owner and there are two defensible ones: keep the `AGREE` and
attach a note, or add a distinct verdict. This lane's recommendation is the
**note**, not a new verdict — the arms genuinely did agree, and demoting it to
unadjudicated would lose real information. Suggested shape, in `adjudicate`'s
`cvkey = hskey` branch, appended to `ADJ_NOTE`:

```
[oracle itself reported failed=N of N: this row is agreement on a fixture that
 is RED on HotSpot 25 on this host, and is not evidence that the VM is correct]
```

with the `failed == tests` case called out separately from `failed < tests`.

---

### NOMINATION D — `regression-suite/corpus/run-corpus.sh`, make re-adjudication a subcommand

The replay in §1 lives in a scratch directory and will not survive the session,
yet it is the cheapest instrument in this whole area: it re-scores every stored
run against the current adjudication code in about ten minutes with **no VM, no
oracle and no rebuild**. Every future change to `arm_key`, `classify_arm` or
`oracle_vacuous` should be run over `out/` before it lands, for exactly the
reason this record exists — the previous key fix was believed for a while and
was measurably ineffective on the real logs.

Proposed: `bash run-corpus.sh readjudicate [<run-dir>…]`, defaulting to every
directory under `$OUT` that has a `results.tsv`, printing
`run / class / old / new / changed / cause`. Two behaviours are load-bearing and
must not be "simplified":

* an `rc=124` row must be classified with a **non-numeric** end-epoch, so it
  reports `TIMEOUT-UNKNOWN`. Passing `0` silently reports every stored timeout
  as `TIMEOUT-BUSY`;
* a stored `hs_state=SKIPPED` must be **preserved**, not re-derived from an
  absent oracle log, or `--no-oracle` rows turn into `ORACLE-UNUSABLE`.

The full working script is reproduced below so it survives this session.

```sh
#!/usr/bin/env bash
set -u
set -o pipefail
CORPUSDIR="<repo>/regression-suite/corpus"
LIB="$(dirname "$0")/driver-lib.sh"
# run-corpus.sh is entirely function definitions plus a single `main "$@"` on
# its LAST line. Dropping that one line yields a sourceable library of the exact
# functions cmd_run uses. Nothing here is a paraphrase.
grep -v '^main "\$@"$' "$CORPUSDIR/run-corpus.sh" > "$LIB"
. "$LIB"

kind_of() {
  case "$1" in
    bc-java|commons-math|spring-framework|hibernate|keycloak|tomcat) echo junit ;;
    *) echo main ;;
  esac
}

printf 'run\tclass\told_verdict\tnew_verdict\told_cv_state\tnew_cv_state\tnew_hs_state\tchanged\tcause\n'
for rd in "$CORPUSDIR"/out/*/; do
  run="$(basename "$rd")"; tsv="$rd/results.tsv"
  [ -f "$tsv" ] || { echo "### $run: NO results.tsv" >&2; continue; }
  corpus="$(sed -n 's/^# corpus=\([^ ]*\).*/\1/p' "$tsv" | head -1)"
  cap="$(sed -n 's/^# timeout=\([0-9]*\)s.*/\1/p' "$tsv" | head -1)"; [ -n "$cap" ] || cap=0
  CORPUS_KIND="$(kind_of "$corpus")"; export CORPUS_KIND
  while IFS=$'\t' read -r cls oldv ocvs ocvrc ocvms ohss ohsrc ohsms onote; do
    case "$cls" in ''|'#'*|class) continue ;; esac
    cvlog="$rd/$cls.cv.log"; hslog="$rd/$cls.hs.log"
    [ -f "$cvlog" ] || cvlog=/dev/null
    [ -f "$hslog" ] || hslog=/dev/null
    # NA end-epoch => stored timeouts classify UNKNOWN rather than being guessed.
    ncvs="$(classify_arm "$ocvrc" "$cvlog" "$cap" NA)"
    # --no-oracle rows have no oracle answer at all; preserve the recorded state.
    if [ "$ohss" = SKIPPED ]; then nhss=SKIPPED
    else nhss="$(classify_arm "$ohsrc" "$hslog" "$cap" NA)"; fi
    adjudicate "$ncvs" "$ocvrc" "$cvlog" "$nhss" "$ohsrc" "$hslog" -1 "$ocvms" "$ohsms" "$cap"
    changed=no; [ "$oldv" != "$ADJ_VERDICT" ] && changed=YES
    cause=""
    if [ "$changed" = YES ]; then
      if [ "$oldv" = DIVERGE ] && [ "$ADJ_VERDICT" = AGREE ]; then
        if has_hook_end "$hslog" && ! has_hook_end "$cvlog"; then cause="hook-END-in-key"; else cause="key-change-other"; fi
      elif [ "$ADJ_VERDICT" = ORACLE-VACUOUS ]; then cause="oracle_vacuous-new-rule"
      elif [ "$ADJ_VERDICT" = CV-TIMEOUT-UNKNOWN ]; then cause="timeout-taxonomy-relabel(unresolved)"
      else cause="other"; fi
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$run" "$cls" "$oldv" "$ADJ_VERDICT" "$ocvs" "$ncvs" "$nhss" "$changed" "$cause"
  done < "$tsv"
done
```

---

## 8. The lesson, for the row after this one

C8's §7 said a harness defect is indistinguishable from its subject's defect
unless the harness carries a control that can only pass when the harness works.
This lane adds the corollary about *corrections*:

**A correction is as quotable as the thing it corrects, and it inherits none of
its evidence.** `P4A` §5's "11 of 13 agree" was a *correction* to a raw
`AGREE=0`, it was published with a table of per-suite counts, and four of those
counts had never been read off a log. The direction of the error was right and
the rows were wrong. The rule that would have caught it is mechanical: **if a
row's number is not in a file you can point at, it is not a number.** Every
figure in this record resolves to `readj-final.tsv` and to a `.cv.log`/`.hs.log`
pair on disk.
