# `OnBeanCondition$Spec` intermittently sees `@ConditionalOnMissingBean` as absent via `MergedAnnotations.get(Class)`, even though `AnnotatedTypeMetadata.isAnnotated(String)` correctly finds it moments earlier

**Status: OPEN — found 2026-07-23.** Residual of the (fixed, this same session)
`module/spring-boot-security` discovery-issue cluster — see
[`../../internal/fixed-suite-bugs/springboot/springboot-security-modifiedclasspathextension-pathing-jar-classpath-manifest-FIXED.md`](../../internal/fixed-suite-bugs/springboot/springboot-security-modifiedclasspathextension-pathing-jar-classpath-manifest-FIXED.md).
Blocks `ManagementWebSecurityAutoConfigurationTests` and
`ReactiveManagementWebSecurityAutoConfigurationTests` (`module/spring-boot-security`),
both only reachable for the first time after that fix landed (previously
masked by the discovery-issue bug, same as the already-closed
`isolated-loader-onbeancondition-type-deduction-bypass-FIXED.md` residual —
this is a **different** mechanism, source-confirmed, not just the same
symptom string recurring).

## Symptom

Both classes' `securesEverythingElseWhenHealthIsAbsent()` test method
(`@ClassPathExclusions(packages = "org.springframework.boot.health.actuate.endpoint")`,
driving `ModifiedClassPathExtension`'s isolated-classloader mechanism) fails:

```
java.lang.IllegalStateException: Error processing condition on org.springframework.boot.security.autoconfigure.web.servlet.ServletWebSecurityAutoConfiguration$EnableWebSecurityConfiguration
  ...
Caused by: java.lang.IllegalStateException: @ConditionalOnMissingBean did not specify a bean using type, name or annotation
  org.springframework.boot.autoconfigure.condition.OnBeanCondition$Spec.validate(OnBeanCondition.java:655)
  org.springframework.boot.autoconfigure.condition.OnBeanCondition$Spec.<init>(OnBeanCondition.java:603)
  org.springframework.boot.autoconfigure.condition.OnBeanCondition.getMatchOutcome(OnBeanCondition.java:147)
```

`ServletWebSecurityAutoConfiguration$EnableWebSecurityConfiguration` is
annotated `@ConditionalOnMissingBean(name = BeanIds.SPRING_SECURITY_FILTER_CHAIN)`
— `BeanIds.SPRING_SECURITY_FILTER_CHAIN` is a `public static final String`
constant (`"springSecurityFilterChain"`), inlined by javac into the
annotation's constant-pool value at compile time — so this should always
report exactly one explicit `name`, never hit the "nothing specified" branch.

## Root cause (source-confirmed, not yet fixed)

`OnBeanCondition.getMatchOutcome` (`OnBeanCondition.java:127-155`) gates entry
into the `ConditionalOnMissingBean` branch with a **string-name-based** check:

```java
if (metadata.isAnnotated(ConditionalOnMissingBean.class.getName())) {
    Spec<ConditionalOnMissingBean> spec = new Spec<>(context, metadata, annotations,
            ConditionalOnMissingBean.class);
    ...
}
```

`Spec`'s constructor then does a **`Class`-object-based** lookup on the exact
same `metadata`/`annotations` pair:

```java
MergedAnnotation<A> annotation = annotations.get(annotationType); // annotationType == ConditionalOnMissingBean.class
```

Added a temporary diagnostic (`OnBeanCondition.java`, throwaway, not part of
any fix, reverted before the CratonVM-side commit) that dumps
`annotation.isPresent()` and the full attributes map on every `Spec`
construction to `C:/craton/onbean-trace.log`. Across one run of
`ManagementWebSecurityAutoConfigurationTests`, `EnableWebSecurityConfiguration`'s
`@ConditionalOnMissingBean` condition is evaluated **10 times** (once per
config-class pass across the test's several `ApplicationContextRunner`
invocations): **9 of the 10 times** `annotation.present=true` with the
correct `name=["springSecurityFilterChain"]` recovered both via
`MergedAnnotations` and a direct `Method.invoke` on the synthesized
annotation proxy; **exactly once**, for the identical class/annotation pair,
`annotation.present=false attributes={}` — i.e. `annotations.get(ConditionalOnMissingBean.class)`
reports the annotation totally absent, even though `metadata.isAnnotated(name)`
(evaluated microseconds earlier, in the same call, on the same `metadata`
object) had just returned `true` to even reach this code.

**This is not a deterministic "always broken" gap** — the annotation
metadata resolves correctly the overwhelming majority of the time, for the
exact same class, exact same annotation, exact same test run. That points at
a **Class-identity split** consistent with this investigation's other
findings (see
[`../../internal/fixed-suite-bugs/springboot/modifiedclasspath-aether-network-hang-cluster-FIXED.md`](../../internal/fixed-suite-bugs/springboot/modifiedclasspath-aether-network-hang-cluster-FIXED.md)'s
"root cause was classloader-identity" section and its sibling residual
docs): `metadata.isAnnotated(String)` matches purely on the annotation's
**binary name**, always correct regardless of which `ClassLoader`'s copy of
`ConditionalOnMissingBean` is asking. `MergedAnnotations.get(Class<A>)` (or
CratonVM's underlying `Class`/annotation-metadata support backing it) likely
compares against a **specific `Class` object** — plausibly the one captured
when `EnableWebSecurityConfiguration`'s annotation metadata was first parsed
(via ASM-style bytecode scanning, independent of any classloader) versus the
`ConditionalOnMissingBean.class` literal as resolved through `OnBeanCondition`'s
**own** defining loader at the moment `Spec`'s constructor runs. Under
`ModifiedClassPathExtension`'s recursive re-execution,
`OnBeanCondition`/`ConditionalOnMissingBean` themselves are also reloaded
through the isolated `ModifiedClassPathClassLoader` (they are not excluded by
this test's narrow `org.springframework.boot.health.actuate.endpoint`
exclusion, but isolation still means a fresh copy is defined) — if exactly
one evaluation races against an in-flight reload/refresh of that isolated
copy (or hits a transiently stale cached `Class` reference before a GC or a
redefinition settles), the by-name check still succeeds while the
by-`Class`-object check would momentarily miss.

Not yet confirmed by attaching a debugger or tracing CratonVM's own
`MergedAnnotation`/`Class.getAnnotation` native implementation directly (only
the Java-level symptom was captured, via the file-based diagnostic above —
`System.err`/`System.getenv` based diagnostics were tried first and produced
**no output at all** for unrelated reasons: this worktree's cached pathing
jar initially pointed at a **different** worktree's `apps/spring-boot` copy,
see the note below; after fixing that, `System.err.println`-based tracing
*still* produced no output, while an unconditional `java.io.FileWriter`
append to a fixed path worked immediately — worth remembering as its own
lesson if picking this up again: don't assume a `System.err`/`getenv()`-based
diagnostic reaches the captured log under `ModifiedClassPathExtension`'s
isolated-loader re-execution; write to a file directly instead).

## Suggested next step

Add native-side tracing (a new `CRATONVM_DBG_*` gated `eprintln!`, following
this repo's established pattern) to whichever function backs
`Class.isAnnotationPresent`/`getAnnotation`/`getDeclaredAnnotations` for
`EnableWebSecurityConfiguration` and `ConditionalOnMissingBean` specifically,
capturing the `Class` object identity (pointer/`ClassId`) on **every** call
across a full run of `ManagementWebSecurityAutoConfigurationTests`, to catch
the one failing evaluation red-handed and see exactly which `ClassId` it
resolved `ConditionalOnMissingBean.class` to versus what the annotation
metadata was actually keyed on. `metadata.isAnnotated(String)` vs
`annotations.get(Class)` disagreeing on the SAME `metadata` object in the SAME
call is the sharpest lead: find where `MergedAnnotations`' internal cache/index
is populated (likely once, lazily, the first time `EnableWebSecurityConfiguration`'s
annotations are scanned) and check whether it can observably race against a
loader-namespace refresh.

## Repro

Worktree `CratonVM-springboot-security-residuals-20260723`, branch
`fix/springboot-security-residuals-20260723`, binary
`cratonvm-springboot-security-residuals.exe`. Both classes reproduce
reliably (100% across every rerun in this session) via:

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe <exe> -JdkHome $env:JAVA_HOME `
  -ClassList apps\spring-boot-suite-runner\repro-security-residuals.tsv -Parallel 4 -TimeoutSec 300
```

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.ReactiveManagementWebSecurityAutoConfigurationTests` |
