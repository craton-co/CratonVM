# `module/spring-boot-security`'s 4 residual classes: isolated `ModifiedClassPathClassLoader` had an effectively empty classpath under a "pathing JAR" launch — FIXED

**Status: FIXED — 2026-07-23.** Closes the 4-class `module/spring-boot-security`
residual from the 2026-07-23 rerun (`apps/spring-boot-suite-runner/RESULTS-20260723.md`):
`SecurityFilterAutoConfigurationEarlyInitializationTests`, `PathRequestTests`,
`ManagementWebSecurityAutoConfigurationTests`,
`ReactiveManagementWebSecurityAutoConfigurationTests` — all 4 share the same
one test method each annotated `@ClassPathExclusions`, all failing with the
exact same discovery-issue shape.

## Symptom

Each class's single `@ClassPathExclusions`-annotated test method failed
(everything else in the class passed normally):

```
ERROR [org.junit.platform.launcher.core.DiscoveryIssueNotifier] TestEngine with ID 'junit-jupiter' encountered a critical issue during test discovery:
(1) [ERROR] UniqueIdSelector [uniqueId = [engine:junit-jupiter]/[class:...]/[method:...]] could not be resolved
```

...regardless of the failing method's parameter arity (0-arg
`toH2ConsoleWhenNoWebServerContextClassPresent()`/`securesEverythingElseWhenHealthIsAbsent()`,
1-arg `testSecurityFilterDoesNotCauseEarlyInitialization(CapturedOutput)`) —
a **different** symptom than the same 4 classes' previously-FIXED
`NoSuchBeanDefinitionException: ObjectProvider<X>` residual (see
[`isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md`](isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md)),
confirming this is a fresh regression in the same isolated-classloader family,
not a recurrence of the old bug.

## Root cause

`ModifiedClassPathExtension.interceptMethod` (Spring's own JUnit 5 extension
backing `@ClassPathExclusions`) builds an isolated `ModifiedClassPathClassLoader`,
sets it as the thread's context classloader, then runs a **brand-new, nested**
`Launcher.discover()`/`.execute()` selecting the exact same test by
`UniqueId`. That nested discovery needs to resolve both the test class and
its one target method through the isolated loader.

`module/spring-boot-security`'s Gradle test classpath is long enough that
`apps/spring-boot-suite-runner`'s runner writes a **"pathing JAR"** — a jar
containing no class files, only a `../../../../apps/META-INF/MANIFEST.MF` `Class-Path:`
attribute listing the real dependency jars/dirs — and launches
`cratonvm.exe --jar <pathing-jar>` instead of a long `-cp` argument (see
`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`'s "Classpath
model"). `vm-cli`'s one-time `--jar` bootstrap handling
(`ClassPath::resolve_class_path`, `vm-cli/src/main.rs`) already expanded such
a manifest `Class-Path:` for the process's own initial/global classpath —
but `classloading/src/class_path.rs`'s **general-purpose** `ClassPath::new`
— the constructor used to build an ad hoc classpath view for **any**
`URLClassLoader`, including `ModifiedClassPathClassLoader` — never did.
Spring's `ModifiedClassPathClassLoader` factory derives its own URL list
from `getURLs()` on the original classloader (which, for a `--jar
<pathing-jar>`-launched process, reports just that one pathing-jar URL) and
filters out excluded entries; since none of the exclusion patterns matched
the pathing jar's own filename, the isolated loader ended up with `paths =
[<pathing-jar>]` — and `ClassPath::new` treated it as an ordinary
(non-fat, non-class-containing) jar, silently resolving to an effectively
**empty** classpath. The isolated loader could therefore never find (or
define) the very test class `ModifiedClassPathExtension` needed to
re-resolve, regardless of the failing method's signature — matching the
uniform, arity-independent failure exactly.

**Performance follow-up bug, found immediately after the primary fix**: once
`ClassPath::new` correctly expands a pathing jar's `Class-Path:` (219 real
entries for this module), `native-builtins/src/classloader.rs`'s
`ucl_try_define_local_class`/`loader_local_resource_urls` were rebuilding a
**fresh** `ClassPath` — reopening and re-indexing all 219 jars from disk —
on **every single** class/resource lookup through the isolated loader
(confirmed via `CRATONVM_DBG_CLASSPATH`: 262 `ClassPath::new` calls in one
single-test-method run, ~100-250ms each). A `URLClassLoader`'s URL list is
fixed at construction, so this is safe to cache once per loader instance.

**Bootstrap-appended visibility gap, found investigating a newly-exposed
Mockito failure**: once class definition worked and tests actually ran far
enough to construct `@Mock` fields, Mockito's inline mock maker failed with
`NoClassDefFoundError: org.mockito.internal.creation.bytebuddy.MockMethodAdvice`.
`classloader_real.rs`'s `loadClass`/`findClass` entry points treat an
isolated `URLClassLoader`'s local-classpath miss as authoritative
(`ClassNotFoundException`) without checking whether the class was
dynamically appended to the **bootstrap** search
(`Instrumentation.appendToBootstrapClassLoaderSearch`, which Mockito's
self-attach mechanism uses for `MockMethodDispatcher`/`MockMethodAdvice`) —
even though a real isolated `URLClassLoader`'s parent is `null`, i.e. the
bootstrap loader itself, so a bootstrap-appended class IS visible to every
loader via ordinary parent delegation. A sibling condition
(`scoped_user_chain` in the same file) already excluded
`is_bootstrap_appended_class` from an analogous check; the isolated-loader
branches were missing it.

## Fix

- **`classloading/src/class_path.rs`**: extracted the per-token classpath
  resolution loop (wildcard expansion, `!/` jar-subdir spec, directory/jmod/
  jimage/plain-jar handling) out of `ClassPath::new` into a shared
  `process_classpath_token(raw, entries, depth)`. A plain (non-fat) jar's own
  manifest `Class-Path:` attribute is now expanded via this same function
  (`load_jar_data_at_depth`, depth-capped at 16 to guard against a cyclic
  `Class-Path` chain), mirroring what real JVM launchers honour for any jar,
  not just the process's initial one.
- **`native-builtins/src/classloader.rs`**: added `cached_loader_class_path`,
  a per-loader-namespace-id (never a raw, GC-movable `ObjectRef` pointer)
  cache of the built `ClassPath`, used by both `loader_local_resource_urls`
  and `ucl_try_define_local_class`.
- **`native-builtins/src/classloader.rs` / `classloader_real.rs`**: the
  isolated-loader "local classpath miss → authoritative `ClassNotFoundException`"
  branches (in `ucl_try_define_local_class`, the real-JDK-mode `loadClass`
  entry point, and `ucl_real_find_class`) now also check
  `cratonvm_classloading::is_bootstrap_appended_class` and defer to the
  caller's normal fallback (which searches the bootstrap path) instead of
  making the miss authoritative, when true.
- **`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`** (tooling,
  not CratonVM itself): `Invoke-Gradlew` now `Push-Location`s into
  `gradlew.bat`'s own directory before invoking it — `gradlew.bat` relies on
  the caller's current directory being the Gradle project root and does not
  `cd` into its own location, so `-Setup`/`-RefreshClasspaths` previously
  failed with "Directory '<caller's cwd>' does not contain a Gradle build"
  whenever invoked from outside `apps/spring-boot`.

## Verified

Worktree `CratonVM-springboot-security-residuals-20260723`, branch
`fix/springboot-security-residuals-20260723`, binary
`cratonvm-springboot-security-residuals.exe`, built from a fully
self-contained checkout (own `apps/spring-boot` copy, own regenerated
pathing jar — not cross-referencing another worktree).

- `PathRequestTests`: **4/4 PASS** (was FAIL on discovery-issue).
- `SecurityFilterAutoConfigurationEarlyInitializationTests`: discovery-issue
  gone; now runs the real test body end to end (starts a real embedded
  Tomcat) and fails on a genuinely different, unrelated assertion — see
  [`../../known-issues/springboot/securityfilterautoconfig-capturedoutput-password-not-observed.md`](../../known-issues/springboot/securityfilterautoconfig-capturedoutput-password-not-observed.md).
- `ManagementWebSecurityAutoConfigurationTests` /
  `ReactiveManagementWebSecurityAutoConfigurationTests`: discovery-issue
  gone; the Mockito `NoClassDefFoundError` (confirmed present before the
  bootstrap-appended-visibility fix, confirmed gone after) is also gone; both
  now fail on a separate, real `OnBeanCondition`/`MergedAnnotations`
  intermittent identity bug — see
  [`onbeancondition-mergedannotations-intermittent-identity-mismatch-FIXED.md`](onbeancondition-mergedannotations-intermittent-identity-mismatch-FIXED.md) (now FIXED).
- `cargo test -p cratonvm-classloading --release class_path`: 89 passed / 0
  failed / 3 ignored (no regressions from the `ClassPath::new` refactor).
- `cargo test -p cratonvm-native-builtins --release classloader`: 105
  passed / 0 failed (no regressions from the caching + bootstrap-appended
  fixes).
- No new/different failures across 3 independent full reruns of all 4
  target classes (`verify-fix2`, `verify-fix3`, `final-verify`) — the 2
  remaining residuals reproduce identically and deterministically each time.

## Suggested next step (regression sweep, not yet run this session)

Re-run the full `docs/internal/fixed-suite-bugs/springboot/modifiedclasspath-aether-network-hang-cluster-FIXED.md`
"Affected classes" table (~29 classes across many modules, all sharing the
`ModifiedClassPathExtension` trigger) to confirm the pathing-jar-specific
fix here doesn't regress any of them — none of those classes' modules were
confirmed to need a pathing jar during the original 07-19 investigation, so
this fix's effect on them is very likely neutral, but not yet verified
directly.

## Affected classes (now passing or narrowed to a distinct residual)

| Module | Class | Outcome |
|---|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.web.servlet.PathRequestTests` | **PASS** |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests` | narrowed — see `securityfilterautoconfig-capturedoutput-password-not-observed.md` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests` | narrowed — see `onbeancondition-mergedannotations-intermittent-identity-mismatch.md` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.ReactiveManagementWebSecurityAutoConfigurationTests` | narrowed — see `onbeancondition-mergedannotations-intermittent-identity-mismatch.md` |
