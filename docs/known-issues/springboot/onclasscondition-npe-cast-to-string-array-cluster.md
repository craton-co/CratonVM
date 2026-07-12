# `OnClassCondition.addAll`: a `NullPointerException` object is handed back where a `String[]` annotation attribute is expected — 75 classes, 348 occurrences (the largest single cluster in the run)

**Status: OPEN, precisely characterized (exact Spring call site pinned).
Severity: CRITICAL — single largest FAIL contributor in the whole suite,
likely a major reason `@ConditionalOnClass`-gated auto-configuration breaks
broadly. Probably the same root-cause family as the already-known, still-open
[[reference_spring_boot_functional_suite]] "SB-04" bug.**

Found while triaging `FAIL`s from the first full Spring Boot suite run (see
[[project_spring_boot_suite_runner_20260711]]). **75 classes, 348 total
occurrences** — across essentially every autoconfigure module (`jackson`,
`jersey`, `tomcat`, `cloudfoundry`, `jdbc`, `grpc`, `task`, …) — hit the
identical, oddly specific error:

```
java.lang.ClassCastException: java.lang.NullPointerException cannot be cast to [Ljava.lang.String;
	at org.springframework.boot.autoconfigure.condition.OnClassCondition.addAll(OnClassCondition.java:133)
	at org.springframework.boot.autoconfigure.condition.OnClassCondition.getCandidates(OnClassCondition.java:124)
	at org.springframework.boot.autoconfigure.condition.OnClassCondition.getMatchOutcome(OnClassCondition.java:90)
```

This is always three levels deep, wrapped by Spring as:
```
IllegalStateException: Error processing condition on <SomeAutoConfiguration>
  -> BeanDefinitionStoreException: Failed to process import candidates for configuration class [...] / Failed to parse configuration class [...]
    -> IllegalStateException: Unstarted application context [...]  (test-level, via AssertJ)
```
— which is why this single root cause fans out into the "Unstarted
application context" / "Failed to parse configuration class" / "Failed to
process import candidates" note-column buckets that otherwise looked like
noise across ~150-200 of the run's 508 FAILs.

## Root cause (Spring-side, pinned)

`OnClassCondition.getCandidates` reads a `@ConditionalOnClass`/
`@ConditionalOnMissingClass` annotation's `value`/`name` attribute (declared
`Class[]`/`String[]`) via `MergedAnnotation` reflection, then
`OnClassCondition.addAll` merges the result into a candidate list — expecting
either `null` or a `String[]`. **Something is handing back a live
`NullPointerException` *object* instead** — not a `ClassCastException` from a
genuinely wrong type, but literally an exception instance sitting where an
array reference belongs, i.e. the annotation-attribute-value slot has been
aliased with an exception-object slot somewhere in CratonVM's reflection/
annotation machinery.

This is the same general shape as two already-documented CratonVM bug
families in this codebase:
- [[reference_native_pending_return_stale_exception_override]] (FIXED
  `b90702a6`) — a native call's leftover pending-exception GC root bled into
  an unrelated value at certain unwind sites. That fix targeted *top-level
  uncaught-exception reporting*; this manifests inside ordinary
  reflection-driven control flow (an annotation attribute read), so it is
  either a different call site with the same underlying mechanism, or a
  residual the original fix didn't cover.
- The still-**OPEN** SB-04 finding in [[reference_spring_boot_functional_suite]]:
  "`MergedAnnotation.getValue(name, Object.class)` returns a SCALAR Class for
  an array-typed annotation attribute (HotSpot: Class[])" — exhaustively
  narrowed there to "an interpreter value-flow issue on the actual object
  graph that doesn't reproduce standalone." This finding gives that
  investigation a **much more specific, standalone-reproducible entry
  point** than SB-04 had: `OnClassCondition` (`spring-boot-autoconfigure`)
  reading `@ConditionalOnClass(name = {...})` is a small, self-contained
  repro surface compared to the original Spring `@Configuration`/CGLIB
  object graph SB-04 was chasing.

## Suggested next step

Reproduce standalone: a plain (non-Spring-Boot) class with
`@ConditionalOnClass(name = "some.missing.Class")` evaluated directly through
`OnClassCondition`/`ConditionEvaluator`, instrumented to dump the actual
runtime type of the value `MergedAnnotation.getValue("name", Object.class)`
(or whatever `OnClassCondition` calls internally — check
`spring-boot-autoconfigure-2.x`'s `OnClassCondition.java:90-135` for the
exact accessor) returns immediately before the cast. Cross-reference against
SB-04's existing instrumentation notes in
`vm_exec.rs::annotation_proxy_invoke`/`elements`/`adapt_annotation_value_for_map`
before starting fresh.

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row for module/spring-boot-jersey's JerseyAutoConfigurationDefaultFilterPathTests, or any of the other 74 affected classes> `
  -Start 1 -Count 1 -Exe <cratonvm exe>
```
