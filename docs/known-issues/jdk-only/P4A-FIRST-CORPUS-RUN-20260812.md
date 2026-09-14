# The first corpus run under `--jdk-only`

**2026-08-12. Fourteen real H2 test classes, adjudicated against HotSpot 25 on
the same host, same session.** This is the measurement the roadmap gates Phase 2
on, and it had never been taken.

```
bash regression-suite/corpus/run-corpus.sh run h2 \
    --classes-from <14 discovered classes> \
    --cv <release binary from this branch> --mode jdk-only --timeout 200
```

## Result

```
corpus=h2 mode=jdk-only   AGREE=10  DIVERGE=2  CV-BROKEN=2  UNADJUDICATED=0
```

**Ten of fourteen real H2 JDBC test classes run to completion under `--jdk-only`
and agree with HotSpot.** Before this session, `java.sql.SQLException` was
unloadable and none of them could start at all.

The four that do not:

| class | verdict | detail |
|---|---|---|
| `org.h2.test.db.TestAlterSchemaRename` | DIVERGE | ran on both, markers differ |
| `org.h2.test.db.TestCases` | DIVERGE | ran on both, markers differ |
| `org.h2.test.db.TestAnalyzeTableTx` | CV-TIMEOUT | killed at 200 s; HotSpot RAN |
| `org.h2.test.db.TestCompatibility` | CV-TIMEOUT | killed at 200 s; HotSpot RAN |

**Do not read the two timeouts as slowness.** The runner says so itself, and the
project has the scar: on this VM a timeout is very often a SIGSEGV that printed
no result line. Both need a stack, not a bigger `--timeout`.

## Why this matters more than the probe sweep

The probe sweep (`P1-RESULT-20260812.md`) is 54 checks I wrote, and it is
54/54. This is fourteen workloads **someone else** wrote, against a real
database engine, and it is 10/14. The gap between those two numbers is the
entire reason the roadmap says a suite number licenses almost nothing.

Neither instrument replaces the other:

* The probes are **diagnostic** — each names one fabricated class and one
  mechanism, so a failure is immediately actionable.
* The corpus is **adjudicative** — it cannot tell you *why*, but it is the only
  thing that can tell you *whether*.

Phase 2 retires native bridges by family and lets the corpus decide whether a
retirement broke anything. That is now possible for the first time. The four
failures above are the baseline any Phase 2 retirement must not make worse.

## Honest limits of this run

* **Fourteen classes, one corpus.** H2's suite is 217 discovered classes; this
  is the first 14 alphabetically, not a sample chosen to be representative.
* **`--mode jdk-only` only.** No `--real-jdk` control arm was run, so it is not
  yet established whether the four failures are strict-mode-specific or
  pre-existing in both modes. **That control is the cheapest next measurement
  and it should be taken before anyone attributes these to strict mode.**
* Nothing here says anything about Spring, Tomcat, Hibernate, Keycloak,
  WildFly, Elasticsearch or Kafka — all inventoried, none run.
* The roadmap's definition of done — a Spring Boot application, a servlet
  container serving HTTPS, and a JDBC workload each running to completion —
  remains unmet. This is the closest anything has come to the third of those.
