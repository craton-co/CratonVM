# `SecurityFilterAutoConfigurationEarlyInitializationTests`: `CapturedOutput` never saw the "Using generated security password" log line — FIXED (by dev drift)

**Status: FIXED — verified 2026-07-26.** Residual of the (fixed)
`module/spring-boot-security` discovery-issue cluster — see
[`springboot-security-modifiedclasspathextension-pathing-jar-classpath-manifest-FIXED.md`](springboot-security-modifiedclasspathextension-pathing-jar-classpath-manifest-FIXED.md).
Originally filed 2026-07-23 as
[`../../known-issues/springboot/securityfilterautoconfig-capturedoutput-password-not-observed.md`](../../known-issues/springboot/securityfilterautoconfig-capturedoutput-password-not-observed.md)
(moved here now that it's closed).

## Symptom (original)

`testSecurityFilterDoesNotCauseEarlyInitialization(CapturedOutput)` ran a
real embedded Tomcat + Spring context end to end but failed:

```
=> org.opentest4j.AssertionFailedError:
Expecting value to be true but was false
  at SecurityFilterAutoConfigurationEarlyInitializationTests.testSecurityFilterDoesNotCauseEarlyInitialization(SecurityFilterAutoConfigurationEarlyInitializationTests.java:84)
```

`password.find()` on `PASSWORD_PATTERN` (`^Using generated security
password: (.*)$`) returned false — the log line
`UserDetailsServiceAutoConfiguration` emits at startup (via
`logger.warn(...)` when `SecurityProperties.User.isPasswordGenerated()` is
true, which it is by default) was confirmed absent from the raw `.out.log`
entirely, not just from `CapturedOutput`'s view of it (ruling out the
sibling `capturedoutput-empty-console-cluster` capture-fidelity family).

## Verification (2026-07-26)

Built a fresh binary from current `dev` (`ce3adcf7f`) in an isolated
worktree (`wt-springboot-authpw-20260726`, no code changes) and reran the
exact repro against a standalone Spring Boot checkout
(`/data/data/springboot-jsonreader-deprecation-20260718`,
`module/spring-boot-security`):

- `SecurityFilterAutoConfigurationEarlyInitializationTests`: **5/5 PASS**.
  Every run logs the generated password
  (`Using generated security password: <uuid>`) and `CapturedOutput` sees
  it; the `TestRestTemplate` basic-auth round trip against the running
  Tomcat instance also succeeds.

Not independently root-caused — resolved as a side effect of unrelated
`dev` drift between 2026-07-23 and 2026-07-26 (a large amount of
classloader/reflection/annotation-metadata work landed in that window,
including the defining-loader-first fixes for nest members/permitted
subclasses, `Class.getResourceAsStream`/`getResource` defining-loader
delegation, and the type-map `authoritative` guard — any of which could
plausibly have touched the isolated-`ModifiedClassPathClassLoader`
condition-evaluation path this test exercises). Exact fixing commit(s) not
bisected. Kept here (rather than deleted) as a record of the symptom in
case it regresses. This is the same resolution shape as the sibling
[`isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md`](isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md).

## Residual status check (related doc)

The doc's own "suggested next step" flagged a plausible connection to
[`../../known-issues/springboot/onbeancondition-mergedannotations-intermittent-identity-mismatch.md`](../../known-issues/springboot/onbeancondition-mergedannotations-intermittent-identity-mismatch.md)
(`OnBeanCondition$Spec` intermittently seeing `@ConditionalOnMissingBean` as
absent). That doc's own two affected classes were independently spot-checked
as part of this verification pass (same binary, same checkout):

- `ManagementWebSecurityAutoConfigurationTests`: **3/3 runs clean, 10/10
  tests** each time — the original `IllegalStateException: ... did not
  specify a bean` never reproduced.
- `ReactiveManagementWebSecurityAutoConfigurationTests`: the original
  exception also never reproduced across ~10 runs, but the class shows a
  separate ~30% intermittent failure — `securesEverythingElseWhenHealthIsAbsent`
  (the same `@ClassPathExclusions`-isolated method the OnBeanCondition doc
  tracks) occasionally fails with
  `IllegalStateException: Timeout on blocking read for 30000000000 NANOSECONDS`
  from `WebFilterChainProxy.filter(...).block(Duration.ofSeconds(30))`.
  A `--stack-dump-on-timeout 20` capture during one such run shows the main
  thread still inside ordinary (if slow) `ApplicationContext` bean creation
  (`ConfigurationPropertyName.buildToString` / property resolution) at the
  20s mark, not stuck on any lock/condition/park — i.e. this looks like the
  same general "isolated-classloader annotation scanning is CPU-bound and
  occasionally exceeds a hardcoded test-side wall-clock budget" pattern
  documented elsewhere in this suite (see the timeout overrides in
  `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`), not a
  recurrence of the annotation-identity bug. Plain HotSpot ran the same
  class 3/3 clean for comparison. Not a new correctness bug filed — a
  concurrent session (title "SpringBoot OnBeanCondition MergedAnnotations
  mismatch") was actively investigating the sibling doc at the time of this
  verification; left that doc untouched rather than duplicate or race its
  in-progress edits.

## Affected classes

| Module | Class | Outcome |
|---|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests` | **PASS** (5/5) |
