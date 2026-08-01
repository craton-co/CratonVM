# `BasicErrorControllerIntegrationTests` comparator crash and residuals

**Status: OPEN — REGRESSED 2026-07-31.** Originally fixed 2026-07-29 and
retired from `docs/known-issues` after direct full-class validation in both
JIT modes. Residual #1 (the `CaseInsensitiveComparator` checkcast abort) has
its own separate, more detailed regression chain, and is now **FIXED
2026-08-01** — see
[`../../internal/fixed-suite-bugs/springboot/springboot-basicerrorcontroller-checkcast-abort-20260731-FIXED.md`](../../internal/fixed-suite-bugs/springboot/springboot-basicerrorcontroller-checkcast-abort-20260731-FIXED.md)
for the GC root cause found 2026-07-31, its subsequent regression the same
day, and the overlay owner-index defect that closed it. Residual #2 (the
`ConditionEvaluationReport` mapping-lambda
corruption, fix #2 below) has now regressed independently — see "Regression
note (2026-07-31)" below — surfacing in a sibling class,
`BasicErrorControllerDirectMockMvcTests`, rather than the originally-reported
class.

## Original symptom

The 2026-07-28 Spring Boot rerun terminated the real HTTP-client integration
path with:

```
NoSuchMethodError
java/lang/String$CaseInsensitiveComparator.apply(Ljava/lang/Object;)Ljava/lang/Object;
caller=org/springframework/http/client/JdkClientHttpRequest.lambda$buildRequest$0
internal error: checkcast: not an object reference
```

`String.CASE_INSENSITIVE_ORDER` is a `Comparator`, not a `Function`; the
observed `apply(Object)` receiver shape was therefore a VM/JIT dispatch
corruption, followed by a fatal invalid-reference `checkcast` during unwind.

## Fixes

Three independently reproduced residuals in the same class were closed.

1. `JdkClientHttpRequest.lambda$buildRequest$0` is now excluded from JIT
   admission. Tiered invokedynamic lowering could otherwise resolve the
   `CaseInsensitiveComparator` receiver as `Function.apply` instead of
   `Comparator.compare`.
2. `ConditionEvaluationReport.lambda$recordConditionEvaluation$0` remains
   interpreted. Its JIT result could be a raw `Object`, corrupting the typed
   `SortedMap<String, ConditionAndOutcomes>`.
3. `AnnotatedTypeMetadata.getAllAnnotationAttributes` remains interpreted.
   Its tiered collector path could dispatch the accumulator as
   `Object.accept(Object,Object)`, producing the same typed-map corruption.
4. The interpreter/native collector fallback in
   `native-collections/src/lib.rs` now recognizes a real
   `Collectors$CollectorImpl` layout and executes its supplier, accumulator,
   and finisher. It no longer substitutes an `ArrayList` for a collector whose
   declared result is Spring's `MultiValueMap`.

The exclusions are exact method-level rules, apply under both conservative and
aggressive policies, and have focused skip-list tests. The surrounding Spring
Boot, HTTP client, and annotation infrastructure remain JIT eligible.

## Validation

Final executable:

```
C:\craton\CratonVM-basic-error-comparator-20260729.exe
SHA-256: 3AC6874FBC1C61F2ACCACE6BA466E4C9CFDAC08C4B719C2F2C08319677EDC748
```

JDK: `C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`.

| Mode | Module and class | Result |
| --- | --- | --- |
| JIT | `module/spring-boot-webmvc` / `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` | `SBRUNNER_RESULT tests=26 failed=0 aborted=0 skipped=0 containersFailed=0` |
| `--nojit` | same | `SBRUNNER_RESULT tests=26 failed=0 aborted=0 skipped=0 containersFailed=0` |
| JIT | direct `String.CASE_INSENSITIVE_ORDER` plus `TreeMap.computeIfAbsent` regression probe | `CASE_INSENSITIVE_COMPARATOR_PROBE_PASS` |
| `--nojit` | same direct probe | `CASE_INSENSITIVE_COMPARATOR_PROBE_PASS` |

The user-supplied `apps/spring-boot` root was not usable for direct evidence:
its generated classpath omitted `spring-boot-4.1.0-SNAPSHOT.jar`, and its
Gradle refresh also lacked `spring-boot-antlib`. The validation used the
complete equivalent Spring Boot 4.1.0-SNAPSHOT fixture at
`C:\craton\CratonVM-spring-boot-rerun-20260717\apps\spring-boot`; this
fixture contains the module jar and the full generated test classpath.

The issue is closed: the original fatal comparator path and all residuals
observed while validating this class are covered by the green full-class
JIT/`--nojit` matrix above.

## Regression note (2026-07-31)

Fix #2 above ("`ConditionEvaluationReport.lambda$recordConditionEvaluation$0`
remains interpreted") relied on the JIT admission exclusion
`SPRINGBOOT-CONDITION-REPORT.1`. That ban was later removed —
`vm/src/jit/skip_list.rs` now carries
`spring_boot_condition_report_mapping_lambda_is_jit_eligible_after_removal`,
a test asserting the method is JIT-eligible "now that
`SPRINGBOOT-CONDITION-REPORT.1` is removed" (commit `0a156db7c`, "fix(jit):
retire all 11 Spring/javac-family JIT bans", part of the
2026-07-30/31 ban-retirement effort documented in
`docs/internal/jit-bans/spring-jit-bans-inventory-and-ban-lift-experiment-20260730.md`).
That doc's own validation of the removal used a narrow standalone probe that
it explicitly flags as **not a valid witness** ("passes on the old tip with
the bans lifted too... the `ConditionEvaluationReport` corruption needs the
full autoconfiguration boot, not merely hot calls into the lambda") plus one
witness class (`BasicErrorControllerIntegrationTests`, 12/12 clean at
`9ac1feffe`) — it did not exercise the sibling class below.

In the same-day 49-class residual rerun
(`craton-rerun-20260731`/`all-jit`, binary built from `dev` @ `9fcd1b63f`,
which has the ban-removal commit `0a156db7c` as an ancestor), the identical
defect reappeared, in the sibling class
`BasicErrorControllerDirectMockMvcTests` rather than the originally-reported
`BasicErrorControllerIntegrationTests`:

```
java.lang.ClassCastException: java.lang.Object cannot be cast to org.springframework.boot.autoconfigure.condition.ConditionEvaluationReport$ConditionAndOutcomes
	at org.springframework.boot.autoconfigure.condition.ConditionEvaluationReport.getConditionAndOutcomesBySource(ConditionEvaluationReport.java:116)
	at org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLogger.logReport(ConditionEvaluationReportLogger.java:61)
	at org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggingListener$ConditionEvaluationReportListener.onApplicationEvent(ConditionEvaluationReportLoggingListener.java:137)
	...
	at org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerDirectMockMvcTests.errorPageAvailableWithParentContext(BasicErrorControllerDirectMockMvcTests.java:80)
SBRUNNER_RESULT tests=4 failed=1 aborted=0 skipped=0 containersFailed=0
```

Log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.error.BasicErrorCo-b9aa5200ce0f.{out,err}.log`
(the test process itself calls `System.exit(1)` after the failure is
recorded, hence the "FAIL" status rather than an abort).

This is exactly the shape fix #2 above eliminated: a raw `Object` surfacing
where `ConditionEvaluationReport`'s typed map expects
`ConditionAndOutcomes`, from the same `lambda$recordConditionEvaluation$0`
whose JIT exclusion was removed. Unlike residual #1 (see the
cross-reference at the top of this doc), no GC fix or other change is known
to have addressed this specific lambda's JIT-compiled behavior, so — absent
evidence otherwise — the ban removal is the direct suspect for this
regression rather than a red herring. Not re-diagnosed at the source level
this session (no source changes made); whoever picks this up should start by
checking what the JIT does differently with `recordConditionEvaluation$0`
compiled vs. interpreted (e.g. `CRATONVM_DBG_JIT_COMPILED=1` plus a
`--nojit` control run of `BasicErrorControllerDirectMockMvcTests`) rather
than assuming it needs the same GC-overlay fix as residual #1.

## Regression note 2 (2026-07-31) — 2 more classes, `SpringBootCondition.recordEvaluation` path (not the logger path)

Also seen in the assigned-class triage of the same 2026-07-31 rerun batch, in
two unrelated modules. Both hit the identical defect through a slightly
different call path than residual #2's regression above — not through
`ConditionEvaluationReportLogger.logReport` →
`getConditionAndOutcomesBySource` (`ConditionEvaluationReport.java:116`), but
directly through `recordConditionEvaluation` itself
(`ConditionEvaluationReport.java:87`, the `SortedMap.computeIfAbsent(...)`
checkcast), called from `SpringBootCondition.recordEvaluation` /
`SpringBootCondition.matches` during `ConditionEvaluator.shouldSkip` while
parsing an autoconfiguration `@Configuration` class — i.e. corruption during
the *write* into the report's map, not only the later *read*:

- `org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests`
  (`module/spring-boot-security-oauth2-resource-server`, run
  `craton-rerun-20260731`/`all-jit`) — 1 of 52 tests failed
  (`autoConfigurationShouldConfigureResourceServerUsingJwkSetUriAndIssuerUri`):
  `BeanDefinitionStoreException: Failed to parse configuration class
  [...OAuth2ResourceServerAutoConfiguration]` ←
  `IllegalStateException: Error processing condition on
  ...OAuth2ResourceServerAutoConfiguration` ←
  `ClassCastException: java.lang.Object cannot be cast to
  ...ConditionEvaluationReport$ConditionAndOutcomes` at
  `ConditionEvaluationReport.recordConditionEvaluation(ConditionEvaluationReport.java:87)`.
  Logs:
  `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/logs/module_spring-boot-security-oauth2-resource-server.org.springframework.boot.security.oauth-2a3a654baeda.{out,err}.log`
- `org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.servlet.CloudFoundryActuatorAutoConfigurationTests`
  (`module/spring-boot-cloudfoundry`, run `craton-hangverify-20260731`/`all-jit`)
  — 1 of 14 tests failed (`cloudFoundryPlatformActive`), same shape:
  `BeanDefinitionStoreException: Failed to parse configuration class
  [...DispatcherServletAutoConfiguration]` ← same
  `ClassCastException` at the same `recordConditionEvaluation` call site.
  Logs:
  `apps/spring-boot-suite-runner/.suite/results/craton-hangverify-20260731/all-jit/logs/module_spring-boot-cloudfoundry.org.springframework.boot.cloudfoundry.autoconfigure.a-7729cc9d2f53.{out,err}.log`

Both are additional evidence for the "regression note (2026-07-31)" theory
above: the JIT-eligibility ban removal (`0a156db7c`) on
`ConditionEvaluationReport.lambda$recordConditionEvaluation$0` is the prime
suspect, and this shows the corruption is not confined to the logger
read-back path or to a single class — it can also corrupt the map during the
original write, failing context startup outright (as a
`BeanDefinitionStoreException`) rather than only failing a later logging
assertion. Not re-diagnosed at the source level this session.

## Affected classes

- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` (original 2026-07-28 report; residual #1 tracked separately, see cross-reference above)
- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerDirectMockMvcTests` (2026-07-31 regression of residual #2)
- `module/spring-boot-security-oauth2-resource-server` — `org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests` (2026-07-31 regression, write-path manifestation via `recordConditionEvaluation` directly)
- `module/spring-boot-cloudfoundry` — `org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.servlet.CloudFoundryActuatorAutoConfigurationTests` (2026-07-31 regression, write-path manifestation via `recordConditionEvaluation` directly)
