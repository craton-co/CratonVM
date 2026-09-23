# Apache Commons Math 4 corpus — non-AGREE census

| | |
|---|---|
| **Measured** | 2026-09-21/22, local Windows checkout, commit `6989206e5`, CratonVM `C:/craton/CVM/target/release/cratonvm.exe` vs oracle HotSpot (openjdk 25.0.3), real JDK 25, all defaults, `run-corpus.sh` real-application-workload harness, `--mode default`, 350 discovered test classes |
| **Census** | AGREE=260, **DIVERGE=5**, CV-BROKEN=0, UNADJUDICATED=85, HARNESS-ERROR=0 |
| **Source** | `regression-suite/corpus/out/commons-math-default-20260922/commons-math-default-20260921-233055/results.tsv`, `commons_math_run_20260922.log` |

**Superseded, same day, before this page's own commit landed:** a parallel fix (`fix(jdk-only): the commons-math corpus's five DIVERGE rows were a missing classpath jar, not a VM defect`, commit `7281ce4b6`, not yet merged to `dev` as of this writing) root-causes **all 5** of the DIVERGE rows below — not just the 3 this page pinned on it — as `corpora.d/commons-math.sh`'s `corpus_classpath()` never adding the `commons-numbers-*`/`commons-rng-*`/`commons-statistics-*`/`commons-geometry-*` jars those modules actually depend on (the file's own header comment said they were "gone from this host entirely," true of the Azure box that note was written on 2026-08-12, not true of this host). With the classpath completed, both arms resolve the same classes with the same eagerness and the divergence goes away. **The "genuinely unexplained" framing below for `AbstractIntegerDistributionTest`/`SingularValueDecompositionTest` turned out to be the same cause, not a second mechanism** — this page's own split was incomplete. Once that fix lands, this page's 5-row analysis is historical, not a live defect list.

UNADJUDICATED=85 is the large, already-understood bucket: this host's `commons-numbers-*`/`commons-rng-*`/`commons-statistics-*`/`commons-geometry-*` dependency jars are evicted entirely (documented in the corpus driver's own header comment), so most classes that depend on them die identically on both arms during JUnit discovery and score `ORACLE-VACUOUS`. Not examined further here — see `corpora.d/commons-math.sh` for that history.

CV-BROKEN=0 means nothing hung or crashed outright. The 5 DIVERGE rows are the only classes needing attention, and reading their actual `.cv.log`/`.hs.log` pairs (not just the `results.tsv` note, which only says "markers differ from HotSpot") splits them into two very different shapes.

## 3 of the 5 are the SAME missing-dependency artifact as UNADJUDICATED — likely mis-scored, not real bugs

| class |
|---|
| `org.apache.commons.math4.legacy.stat.descriptive.StatisticsTest` |
| `org.apache.commons.math4.legacy.stat.descriptive.MultivariateSummaryStatisticsTest` |
| `org.apache.commons.math4.legacy.stat.descriptive.SynchronizedMultivariateSummaryStatisticsTest` |

All three: CratonVM's arm dies outright with `class file error: class not found: org/apache/commons/statistics/descriptive/Sum` (or `SumOfSquares`) — one of the evicted dependency jars — **before printing its own `SBRUNNER_RESULT` line at all**. HotSpot's arm hits the identical missing dependency, but reports it as a per-test `NoClassDefFoundError`/`ExceptionInInitializerError` and still reaches `SBRUNNER_RESULT tests=N failed=N` (all failed, but the marker line exists).

This is a **harness gap, not a CratonVM defect**: `run-corpus.sh`'s `oracle_vacuous` guard (see the corpus driver's own history, lane C8/C17) checks whether the *oracle* (HotSpot) died with no `SBRUNNER_RESULT` and reclassifies that as `ORACLE-VACUOUS`. It does not appear to check the same condition on the **CratonVM** side — here it is the CratonVM arm that dies before any result marker, while HotSpot's happens to survive far enough to print one (all-failing) result. The two arms' marker outputs differ only because they died at *different* points along the same missing-dependency failure, which the verdict engine currently reads as `DIVERGE` rather than the correct `ORACLE-VACUOUS`/`UNADJUDICATED`-equivalent verdict for "both arms are broken by the same environmental gap."

**Action, in order of value:** (1) restore the evicted `commons-numbers`/`commons-statistics` jars to this host's `.m2` so the corpus can actually exercise this code, which would very likely absorb most of the 85 UNADJUDICATED rows too; (2) separately, extend `oracle_vacuous` (or add a `cv_vacuous` sibling check) to catch "CV died with no `SBRUNNER_RESULT`" the same way it already catches the oracle side, so this shape can never again mis-score as `DIVERGE`.

## 2 of the 5 are a genuinely different, unexplained divergence

| class | cv | hs |
|---|---|---|
| `org.apache.commons.math4.legacy.distribution.AbstractIntegerDistributionTest` | `System.exit(1)` called, no `SBRUNNER_RESULT` printed | `SBRUNNER_RESULT tests=1 failed=1` |
| `org.apache.commons.math4.legacy.linear.SingularValueDecompositionTest` | `System.exit(0)` called, no `SBRUNNER_RESULT` printed | `SBRUNNER_RESULT tests=13 failed=2` |

Neither of these shows the missing-dependency `class not found` error the other three do — CratonVM's JUnit launcher process terminates via `System.exit` (once with code 1, once with code 0 — the exit code itself differs between the two, which argues against one single copy-paste cause) without ever printing its own result marker, while HotSpot completes normally and reports specific (non-zero) failure counts. **Not yet root-caused.** Worth a direct repro (`java -cp <cp> SbRunner <class>` under CratonVM alone, watched for exactly where/why `System.exit` gets called instead of the JUnit launcher's normal completion path) before guessing further — this could be anything from an uncaught exception in a shutdown hook to a JUnit-engine discovery quirk specific to these two classes.

## Open items

1. Neither of the two real findings has been root-caused yet; both need a standalone repro outside the corpus harness.
2. The 3-of-5 "missing dependency, wrong verdict" shape is worth fixing at the harness level (`run-corpus.sh`'s `oracle_vacuous`/verdict logic) independently of any CratonVM change, since it currently manufactures false `DIVERGE` rows whenever the CV arm (not the oracle) is the one that dies first.
3. Restoring the evicted commons-numbers/commons-statistics/commons-rng/commons-geometry jars to this host would let a large fraction of the 85 UNADJUDICATED rows actually run for the first time — that is likely higher-value than chasing these 5 rows individually.
