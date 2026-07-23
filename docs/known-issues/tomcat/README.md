# Tomcat suite — known issues index

Split out of `docs/internal/fixed-suite-bugs/tomcat/18-fixture-environment-gaps-20260724.md`
(2026-07-24) into one file per independently-actionable item, so different
sessions can pick separate items up in parallel without stepping on each
other. Source data and the full 35-class categorization with root-cause
detail: that doc, plus `apps/tomcat-suite-runner/RESULTS-20260724-cwdfix.md`
on the Azure host. All of this is against the Linux Tomcat fixture at
`/data/data/tomcat-dohead-fixture-20260717` (symlinked
`/data/data/apps/tomcat`), reusable Linux runner at
`apps/tomcat-suite-runner/run-tomcat-suite.sh`.

## Fixture-completion work (not CratonVM bugs — do these to unblock real testing)

| Doc | Classes | Fix effort |
|---|---:|---|
| [missing-antjar-classpath.md](missing-antjar-classpath.md) | 2 | Trivial — add a jar |
| [missing-httpd-binary.md](missing-httpd-binary.md) | 8 | Small — install + configure `httpd` |
| [largeheap-flat-heap-oom.md](largeheap-flat-heap-oom.md) | 3 | Trivial — exclude or bump per-class `-Xmx` |
| [missing-catalina-localhost-context-configs.md](missing-catalina-localhost-context-configs.md) | 8 | Small — stage 2 XML files |
| [missing-build-lib-jars.md](missing-build-lib-jars.md) | 1 | Small — run `ant package`/`deploy` |
| [unbuilt-virtual-webapp-submodule.md](unbuilt-virtual-webapp-submodule.md) | 1 | Small — `mvn compile` one submodule |

**23 of the 35 non-PASS classes** in this bucket are blocked purely on fixture
work above, not investigation. Completing all six items would very plausibly
turn some into real CratonVM regressions (as happened when the CWD harness
bug fix alone reclassified ~65 previously-miscounted classes) — treat a
PASS after fixture completion as the expected/good outcome, not a surprise.

## Needs investigation, not yet root-caused

| Doc | Classes |
|---|---:|
| [untriaged-oddities.md](untriaged-oddities.md) | 2 |
| [hang-classification-unconfirmed-host-contention.md](hang-classification-unconfirmed-host-contention.md) | 9 |

## Real CratonVM bug (not a fixture gap — despite living in the same 35-class "both VMs fail" bucket)

| Doc | Classes |
|---|---:|
| [value-stack-usize-underflow-nio-worker-panic.md](value-stack-usize-underflow-nio-worker-panic.md) | 2 (confirmed in a 3rd bucket too — see doc) |

Total accounted for: 6 + 2 + 1 = 9 docs covering all 35 non-PASS classes from
the "true fixture gap" bucket, plus the 2 classes where the real panic also
reproduces.
