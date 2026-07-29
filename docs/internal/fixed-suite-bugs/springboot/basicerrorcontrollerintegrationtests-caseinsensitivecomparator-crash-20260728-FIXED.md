# `BasicErrorControllerIntegrationTests` comparator crash and residuals -- FIXED

**Status: FIXED 2026-07-29.** Retired from `docs/known-issues` after direct
full-class validation in both JIT modes.

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
