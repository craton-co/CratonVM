# `SecurityFilterAutoConfigurationEarlyInitializationTests`: `CapturedOutput` never sees the "Using generated security password" log line

**Status: OPEN — found 2026-07-23.** Residual of the (fixed, this same
session) `module/spring-boot-security` discovery-issue cluster — see
[`../../internal/fixed-suite-bugs/springboot/springboot-security-modifiedclasspathextension-pathing-jar-classpath-manifest-FIXED.md`](../../internal/fixed-suite-bugs/springboot/springboot-security-modifiedclasspathextension-pathing-jar-classpath-manifest-FIXED.md).
Only reachable for the first time after that fix landed (previously masked
by the discovery-issue bug).

## Symptom

`testSecurityFilterDoesNotCauseEarlyInitialization(CapturedOutput)`
(`@ClassPathExclusions({"spring-security-oauth2-client-*.jar",
"spring-security-oauth2-resource-server-*.jar",
"spring-security-saml2-service-provider-*.jar"})`, `@ExtendWith(OutputCaptureExtension.class)`)
now runs a real embedded Tomcat + Spring context end to end (confirmed by the
log: `Tomcat started on port ... with context path '/'`) but fails:

```
=> org.opentest4j.AssertionFailedError:
Expecting value to be true but was false
  at SecurityFilterAutoConfigurationEarlyInitializationTests.testSecurityFilterDoesNotCauseEarlyInitialization(SecurityFilterAutoConfigurationEarlyInitializationTests.java:84)
```

Line 84 is `assertThat(password.find()).isTrue()`, where `password` is
`PASSWORD_PATTERN.matcher(output)` and `PASSWORD_PATTERN` is
`^Using generated security password: (.*)$` (multiline). Spring Security's
`UserDetailsServiceAutoConfiguration` logs this line at `INFO` whenever no
explicit user/password is configured — its absence from `output` means
either (a) the line was never logged, or (b) it was logged but not captured
into the `CapturedOutput` the test method receives.

## Narrowed: the line is never logged at all (not a capture bug)

Checked the raw `.out.log` for this exact run: zero occurrences of
"password" anywhere, case-insensitive. So this is **not** a `CapturedOutput`/
`OutputCaptureExtension` capture-fidelity issue (the sibling
`capturedoutput-empty-console-cluster.md` family) — the log line genuinely
never gets emitted, meaning `UserDetailsServiceAutoConfiguration`'s
generated-password bean either isn't being created, or is created but its
`InfoLogger`/`ApplicationListener` that prints the line never fires, under
this test's isolated-`ModifiedClassPathClassLoader` execution.

Not investigated further this session (time was spent on the other
`module/spring-boot-security` residual — see the sibling
`onbeancondition-mergedannotations-intermittent-identity-mismatch.md` doc,
which found a genuine intermittent `MergedAnnotations`/`Class`-identity
mismatch in the same general area: autoconfiguration condition evaluation
under an isolated loader). Plausible, unconfirmed connection: if
`UserDetailsServiceAutoConfiguration`'s own `@ConditionalOnMissingBean`
condition (gating whether the default in-memory user + generated password
get created) intermittently mis-evaluates the same way, the bean simply
never gets created — no exception, no log line, and this test's assertion
would fail exactly as observed with no other symptom.

## Suggested next step

Confirm whether `UserDetailsServiceAutoConfiguration`'s bean methods
actually ran (add a probe: assert on `context.getBeanNamesForType(UserDetailsService.class)`,
or check `ConditionEvaluationReport` for this specific run) rather than only
asserting on the log line. If the bean is missing, chase this via the SAME
lead as the sibling `OnBeanCondition` doc (a `CRATONVM_DBG_*`-gated trace on
`Class.isAnnotationPresent`/annotation-metadata resolution for
`UserDetailsServiceAutoConfiguration`'s conditions) rather than treating it
as a separate investigation from scratch.

## Repro

Worktree `CratonVM-springboot-security-residuals-20260723`, branch
`fix/springboot-security-residuals-20260723`. Reproduces reliably (100%
across every rerun this session):

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe <exe> -JdkHome $env:JAVA_HOME `
  -ClassList apps\spring-boot-suite-runner\repro-security-residuals.tsv -Parallel 4 -TimeoutSec 300
```

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests` |
