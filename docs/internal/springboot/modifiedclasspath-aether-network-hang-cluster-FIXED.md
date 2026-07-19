# `@ClassPathExclusions`/`@ClassPathOverrides` tests HANG — likely fallout of the just-FIXED `String.setOption` wrong-receiver-dispatch fix unmasking a real-network Aether resolution with no reachable repo

**Status: FIXED — 2026-07-19 (core recursion bug); see "Update 2026-07-19 (FIXED)" below. Originally found 2026-07-17.**

## Symptom

4 classes in `core/spring-boot-autoconfigure` never produce a JUnit result —
no `SBRUNNER_RESULT` line, no `Test run finished`, nothing in `.out.log` at
all. Each `.err.log` starts up normally (post-clinit fixups, JUnit engine
discovery) and then consists, for its entire remaining multi-hundred-line
length, of the *same* `cratonvm::gc::guard` warning repeating over and over
on the same 1-2 object addresses, with no other log line ever appearing
again until the suite kills the process at the shard timeout:

```
2026-07-17T21:11:24.959672Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matching receiver type) obj=0x1a65103e580 index=0 num_slots=0 class_id=ClassId(747) class_name=org/junit/jupiter/engine/execution/InterceptingExecutableInvoker real_field_count=Some(0)
2026-07-17T21:11:25.958694Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped ... obj=0x1a651028f30 ... class_name=org/junit/jupiter/engine/execution/InterceptingExecutableInvoker real_field_count=Some(0)
2026-07-17T21:11:26.107690Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped ... obj=0x1a65103e580 ...
```

...continuing at roughly the same cadence for the remainder of the log
(230/232/230/230 lines total per file — cut off by the log-file line cap,
not by the process actually stopping), alternating between exactly the same
two `InterceptingExecutableInvoker`/`InvocationInterceptorChain` object
addresses forever. No `$Proxy`, no other class, no test progress — this is
qualitatively different from the same warning's appearance in every
*passing* class's log (a handful of occurrences during JUnit engine
bootstrap, then the log moves on to real work).

| Class | Log |
|---|---|
| `NonAspectJAopAutoConfigurationTests` | `shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.aop.NonAspectJAopAutoCon-eb1da2b74764.err.log` |
| `ConditionalOnCheckpointRestoreTests` | `shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.condition.ConditionalOnC-99ff570ff2cf.err.log` |
| `ConditionalOnMissingBeanWithFilteredClasspathTests` | `shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.condition.ConditionalOnM-ce7dff77cbc5.err.log` |
| `OnBeanConditionTypeDeductionFailureTests` | `shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.condition.OnBeanConditio-ef3749960ee5.err.log` |

Full logs (paths relative to repo root):
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.{aop.NonAspectJAopAutoCon-eb1da2b74764,condition.ConditionalOnC-99ff570ff2cf,condition.ConditionalOnM-ce7dff77cbc5,condition.OnBeanConditio-ef3749960ee5}.err.log`

## Root cause (hypothesis, not confirmed by attaching a debugger)

All 4 classes' source files import `org.springframework.boot.testsupport.classpath.ClassPathExclusions`
(`NonAspectJAopAutoConfigurationTests`, `ConditionalOnMissingBeanWithFilteredClasspathTests`,
`ConditionalOnCheckpointRestoreTests`) or `ClassPathOverrides`
(`OnBeanConditionTypeDeductionFailureTests`) — confirmed by grepping
`apps/spring-boot/core/spring-boot-autoconfigure/src/test/java/.../{aop,condition}/*.java`
in this worktree. Both annotations drive the `ModifiedClassPathExtension`
JUnit 5 extension, which builds a `ModifiedClassPathClassLoader` per test —
and that classloader's dependency-override/-exclusion resolution goes
through Eclipse Aether (`DefaultArtifactResolver`) making a real Apache
HttpClient HTTP request, per
[`wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`](wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md)'s
"Case 1" investigation (same day, same repo) — which root-caused and fixed
a `NoSuchMethodError: java/lang/String.setOption` crash in exactly this
`ModifiedClassPathClassLoader.resolveCoordinates` → Aether → Apache
HttpClient → `Socket.setSoLinger()` → `SocketImpl.setOption()` call chain.
**`ConditionalOnCheckpointRestoreTests` is explicitly listed as one of the 9
originally-crashing classes that fix's Case 1 covers**, with status "FIXED".

Putting these together: the Case-1 fix stopped the wrong-receiver crash in
`Socket.setOption()`, which means the Aether HTTP client call that used to
crash immediately now runs to completion of that specific call — and
proceeds to make a **real outbound network connection** to resolve/download
a Maven artifact (aspectjweaver for the AOP test's exclusion, a CRaC/other
dependency override for the other three). This suite-runner sandbox has no
reachable Maven repository, so that connection attempt very plausibly hangs
or endlessly retries rather than failing fast — converting what used to be
an instant crash into a full-timeout HANG. This does not contradict the
Case-1 fix (it correctly fixed the described defect); it looks like an
unanticipated side effect of unmasking a code path that was previously only
reachable long enough to crash.

**What is not confirmed:** the exact mechanism connecting the observed log
shape (a tight, roughly-1-second-cadence repeat of the *same* JUnit
interceptor-chain `get_field` guard warning, not literal silence) to a
blocked network read. A single blocked `Socket.connect()`/`read()` would
normally produce no further application-level log lines at all, not a
repeating warning. The most likely bridge: Aether/Apache HttpClient's
connection-pool or retry logic re-enters `ModifiedClassPathClassLoader`'s
JUnit-interceptor-wrapped setup path on each retry attempt (e.g. a fast
connection-refused/DNS-failure retry loop rather than a single multi-minute
TCP timeout), and each retry re-triggers the benign, already-known-safe
`InterceptingExecutableInvoker` OOB-field-read guard once per pass — which
would explain both the repeating warning *and* why the loop never
terminates (each individual attempt fails fast and is retried, forever,
rather than blocking once for a long time). This bridge is a plausible
synthesis of the two known facts (the `ClassPathExclusions`/`ClassPathOverrides`
→ Aether trigger, and the log's literal repeat pattern), not something
directly observed via a debugger — flagged as the strongest next step for
whoever picks this up (attach to one of these 4 processes mid-hang and get
a real thread dump / stack trace to confirm or refute).

## Update 2026-07-17 (same-day, second module, different log signature)

Two more classes HANG the same day in **`module/spring-boot-micrometer-metrics`**,
both annotated `@ConfigureClasspathToPreferLog4j2` — which is itself
`@ClassPathExclusions("log4j-to-slf4j-*.jar")` +
`@ClassPathOverrides({"org.apache.logging.log4j:log4j-core:2.24.3", "org.apache.logging.log4j:log4j-slf4j-impl:2.24.3"})`,
driving the exact same `ModifiedClassPathExtension` mechanism as the 4
classes below:

| Class | Log |
|---|---|
| `logging.log4j2.Log4J2MetricsWithLog4jLoggerContextAutoConfigurationTests` | `shard4/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-12f21366ced9.err.log` |
| `logging.logback.LogbackMetricsAutoConfigurationWithLog4j2AndLogbackTests` | `shard4/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-c49196fdcfa9.err.log` |

**The log signature is different** from the 4 `core/spring-boot-autoconfigure`
classes above: these two `.err.log`s contain only 5 lines total (VM startup
"Post-clinit fixup" messages) and the `.out.log` is completely empty (0
bytes) — no `InterceptingExecutableInvoker`/`InvocationInterceptorChain`
repeating warning at all, just silence immediately after startup. This is
consistent with the same underlying "Aether real-network artifact resolution
with no reachable repo" hypothesis but blocking at an **earlier** point in
`ModifiedClassPathClassLoader`'s setup — plausibly before the JUnit
interceptor-chain wrapping this doc's repeating-warning bridge theory depends
on is ever reached (`@ConfigureClasspathToPreferLog4j2`'s
`@ClassPathOverrides` downloads two full artifacts up front, vs. the 4
classes above which use lighter/single exclusions or overrides — more Aether
work needed before any test method invocation begins, so a hang here would
naturally produce zero interceptor-wrapper log lines rather than a repeating
pattern). Not confirmed — filed as the same cluster on the strength of the
identical `ModifiedClassPathExtension` trigger annotation, flagging the log-shape
difference explicitly rather than asserting an identical mechanism.

## Update 2026-07-17 (separate triage batch, same rerun) — SOE evidence points at recursive self-invocation, not (only) network hang

Two more classes matching the `@ClassPathExclusions`/`@ClassPathOverrides` →
`ModifiedClassPathExtension` trigger were found independently in this same
rerun, in two more modules — raising this cluster to **6 classes across 3
modules**:

| Module | Class | Symptom |
|---|---|---|
| `module/spring-boot-jms` | `ConnectionFactoryUnwrapperTests` (nested `Unwrap.unwrapWithoutJmsPoolOnClasspath`, `@ClassPathExclusions("pooled-jms-*")`) | **`StackOverflowError`**, not a hang — process exits after 154.7s with a full JUnit summary (11/12 other tests pass) |
| `module/spring-boot-gson` | `Gson210AutoConfigurationTests` (class-level `@ClassPathExclusions("gson-*.jar")` + `@ClassPathOverrides(...)`) | HANG — empty `.out.log`, periodic `InterceptingExecutableInvoker` guard warning, killed at shard timeout |

`ConnectionFactoryUnwrapperTests`'s failure is a **genuine, deep**
`StackOverflowError` — **7354 real stack frames** (not the unrelated shallow
3-11 frame `jit-dispatch-depth-guard` cluster), captured in full in:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-jms.org.springframework.boot.jms.ConnectionFactoryUnwrapperTests.out.log`.
The repeating frame group (855/380/380/... occurrences) is:

```
org.junit.platform.launcher.core.EngineExecutionOrchestrator.withInterceptedStreams(...)
org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension.interceptMethod(...)
org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension.interceptTestMethod(...)
org.junit.jupiter.engine.execution.InterceptingExecutableInvoker$ReflectiveInterceptorCall...
...
org.junit.platform.launcher.core.DelegatingLauncher.execute(...)   <- re-enters the SAME nested Launcher.execute cycle
```

**This precisely confirms, at the mechanism level, why the cluster hangs
rather than just crashing** — reading `ModifiedClassPathExtension.java`
(`apps/spring-boot/test-support/spring-boot-test-support/src/main/java/org/springframework/boot/testsupport/classpath/ModifiedClassPathExtension.java`,
this worktree) shows the extension is *designed* to recurse exactly once:
`interceptMethod` builds a new `ModifiedClassPathClassLoader`, sets it as the
thread's context class loader, and launches a **brand-new, in-process**
`Launcher` (`LauncherFactory.create()` → `launcher.discover()` →
`launcher.execute()`) to re-run the very same test — the recursion is
supposed to terminate on the *second* pass via this guard:

```java
private boolean isModifiedClassPathClassLoader(ExtensionContext extensionContext) {
    Class<?> testClass = extensionContext.getRequiredTestClass();
    ClassLoader classLoader = testClass.getClassLoader();
    return classLoader.getClass().getName().equals(ModifiedClassPathClassLoader.class.getName());
}
```

i.e. once the test class is reloaded through a `ModifiedClassPathClassLoader`,
`interceptMethod` should see `isModifiedClassPathClassLoader(...) == true` and
just `invocation.proceed()` instead of spinning up yet another nested
`Launcher`. The `ConnectionFactoryUnwrapperTests` stack trace shows this guard
never tripping — `ModifiedClassPathExtension` → nested `Launcher.execute()` →
`ModifiedClassPathExtension` → nested `Launcher.execute()` repeats **95
times** before the real stack finally overflows. **Hypothesis, not confirmed
by attaching a debugger:** on CratonVM, `isModifiedClassPathClassLoader`'s
name-string comparison (`classLoader.getClass().getName().equals(...)`)
never returns `true` for a class that actually was loaded through a fresh
`ModifiedClassPathClassLoader` instance — plausibly because `testClass.getClassLoader()`
doesn't return the classloader the nested `Launcher` actually used to load it,
or because the freshly-built `URLClassLoader` subclass's `getClass().getName()`
doesn't round-trip correctly through CratonVM's classloading — so every
recursive pass looks like "still the original classpath" and spins up another
nested Launcher forever.

This unifies with the original 4 HANG classes above: if each recursive pass
also does the same slow classpath-scan + JUnit-engine-bootstrap work (which is
what fires the periodic `InterceptingExecutableInvoker` guard warning seen in
all 6 logs), a class whose recursion is slow enough per pass never reaches the
~streamlined~ ~7000-frame~ stack depth before the shard timeout kills it —
presenting as a silent HANG instead of an `StackOverflowError`. Whether the
*original* network-resolution angle (Aether/Apache HttpClient hitting an
unreachable Maven repo) is also a contributing/separate factor for those 4
specific classes is not re-examined here; the two mechanisms are not mutually
exclusive (a slow-but-eventually-failing network call inside one recursive
pass would also explain the timing), but the `ConnectionFactoryUnwrapperTests`
evidence is a *directly observed* recursive-self-invocation bug in
`ModifiedClassPathExtension`'s own guard logic, independent of any network
angle, and should be treated as at least a contributing (if not primary)
mechanism for the whole cluster.

**Discrepancy with the FIXED doc, per this session's triage instructions:**
`ConditionalOnCheckpointRestoreTests` is listed as FIXED (Case 1) in
`wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`, dated the same
day as this rerun. It is **not** passing in this rerun — it HANGs instead of
producing the old crash, which is consistent with "fixed the documented
symptom, but the class still doesn't pass" rather than "the fix didn't
apply." Noted explicitly rather than silently re-filing as a fresh
unrelated bug.

## Update 2026-07-17 (bin5 rerun triage) — 9 more classes across 4 more modules, confirmed via test source (not just log shape)

Grepped the actual test source in this worktree
(`apps/spring-boot/module/.../src/test/java/...`) for
`@ClassPathOverrides`/`@ClassPathExclusions` on every HANG class in this
session's batch, rather than relying on log-shape alone, and got 9
positive hits across `module/spring-boot-security`,
`module/spring-boot-jersey`, `module/spring-boot-cache`, and
`module/spring-boot-liquibase` — none previously listed here:

| Module | Class | Annotation (source-confirmed) | Scope |
|---|---|---|---|
| `module/spring-boot-security` | `...web.servlet.PathRequestTests` | `@ClassPathExclusions(packages = "org.springframework.boot.web.server.context")` | 1 test method |
| `module/spring-boot-security` | `...web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests` | `@ClassPathExclusions({"spring-security-oauth2-client-*.jar", "spring-security-oauth2-resource-server-*.jar", ...})` | 1 test method |
| `module/spring-boot-security` | `...web.servlet.ManagementWebSecurityAutoConfigurationTests` | `@ClassPathExclusions(packages = "org.springframework.boot.health.actuate.endpoint")` | 1 test method |
| `module/spring-boot-security` | `...web.reactive.ReactiveManagementWebSecurityAutoConfigurationTests` | `@ClassPathExclusions(packages = "org.springframework.boot.health.actuate.endpoint")` | 1 test method |
| `module/spring-boot-liquibase` | `Liquibase423AutoConfigurationTests` | `@ClassPathOverrides("org.liquibase:liquibase-core:4.23.1")` | class-level |
| `module/spring-boot-cache` | `EhCache3CacheAutoConfigurationTests` | `@ClassPathExclusions("ehcache-2*.jar")` | class-level |
| `module/spring-boot-jersey` | `JerseyChildManagementContextConfigurationTests` | `@ClassPathExclusions("spring-webmvc-*")` | class-level |
| `module/spring-boot-jersey` | `JerseySameManagementContextConfigurationTests` | `@ClassPathExclusions("spring-webmvc-*")` | class-level |
| `module/spring-boot-jersey` | `JerseyAutoConfigurationTests` | `@ClassPathExclusions("jersey-spring6-*.jar")` | 1 test method |

**The observed log shape varies more than the original 4-class writeup
above, but the trigger mechanism is the same (source-confirmed, not just
inferred from log shape this time):**

- `PathRequestTests` and `SecurityFilterAutoConfigurationEarlyInitializationTests`
  match the original description closely: 225/230 and 224/230 lines of
  their `.err.log` are the same repeating `InterceptingExecutableInvoker`
  `get_field` guard warning at roughly 1-2s cadence for ~2 minutes, then
  silence until the timeout kill. Since the `@ClassPathExclusions`
  annotation is only on **one** test method in each class, this is
  consistent with many earlier test methods running and passing normally
  (each producing its own brief burst of the same benign warning during
  JUnit's per-test setup, which is why the file is dominated by it even
  before the actual hang) before execution reaches the annotated method
  and gets stuck in the Aether-triggered retry/hang pattern.
- `Liquibase423AutoConfigurationTests` (class-level `@ClassPathOverrides`,
  so the *first* test method already triggers it) shows the identical
  224/230-line churn pattern starting almost immediately after VM boot,
  with no other content at all — consistent with the class-level
  annotation meaning there's no "normal" prior test activity to show first.
- `EhCache3CacheAutoConfigurationTests`, both Jersey
  `*ManagementContextConfigurationTests` classes, and
  `ReactiveManagementWebSecurityAutoConfigurationTests` show **zero**
  `InterceptingExecutableInvoker` warnings at all — just the 5 standard
  VM-boot `Post-clinit fixup` lines (plus, for
  `ReactiveManagementWebSecurityAutoConfigurationTests`, one "Mockito is
  currently self-attaching" line — that class also uses Mockito
  independently of the classpath-override mechanism) and then silence.
  This is a **different observable shape** from the churning-warning
  cluster above (no repeating log line at all, not even the benign JUnit
  guard warning) — plausibly because these 4 have the annotation at
  **class level**, so the very first thing the JUnit engine does for the
  class is build the `ModifiedClassPathClassLoader` (before any
  `InterceptingExecutableInvoker`-mediated method invocation has happened
  even once), whereas the two method-level-annotated classes above get
  to "warm up" through several passing methods first. Not confirmed by a
  debugger attach — flagged as a plausible explanation for the shape
  difference, not a demonstrated one.
- `JerseyAutoConfigurationTests` and `ManagementWebSecurityAutoConfigurationTests`
  (both method-level annotations) sit in between: `JerseyAutoConfigurationTests`
  shows one extra line (`"Running in a non-OSGi environment"`) before
  going silent (no OOB-warning churn), while `ManagementWebSecurityAutoConfigurationTests`
  shows extensive real application content first (Tomcat instances
  starting/stopping across many earlier, unrelated test methods, including
  the same benign OOB-warning noise interleaved with real `INFO`-level
  Tomcat lifecycle logging) before going silent once execution reaches its
  one `@ClassPathExclusions`-annotated method.

None of these 9 logs contain any direct evidence of a live network
call (no socket/DNS-related log line — CratonVM does not appear to log
at that level) or a captured thread stack, so — same caveat as the
original 4-class entry above — the *exact* blocking point is still not
confirmed by a debugger attach in this session either. The source-level
confirmation (all 9 do use `@ClassPathOverrides`/`@ClassPathExclusions`,
which is the one thing the original entry's classes and these 9 share)
is new evidence this session adds; the network-hang mechanism itself
remains a strong hypothesis, not a confirmed root cause.

## Update 2026-07-17 (bin8 rerun triage) — 5 more classes across 4 more modules, same source-confirmed trigger

Independently triaged this session's `bin8.tsv` HANG batch (9 classes total)
before reading this doc; grepped each class's real test source for
`@ClassPathExclusions`/`@ClassPathOverrides` (not just log shape) and found 5
positive hits, none previously listed here, spanning 4 additional modules
(`spring-boot-webmvc`, `spring-boot-security-oauth2-authorization-server`,
`spring-boot-validation`, `spring-boot-webservices`):

| Module | Class | Annotation (source-confirmed) | Scope |
|---|---|---|---|
| `module/spring-boot-webmvc` | `...actuate.web.WebMvcEndpointManagementContextConfigurationTests` | `@ClassPathExclusions(packages = "org.springframework.boot.health.actuate.endpoint")` | 1 test method (line 71) |
| `module/spring-boot-security-oauth2-authorization-server` | `...servlet.OAuth2AuthorizationServerAutoConfigurationTests` | `@ClassPathExclusions({"spring-security-oauth2-client-*.jar", "spring-security-oauth2-resource-server-*.jar", ...})` | 1 test method (line 66) |
| `module/spring-boot-validation` | `ValidationAutoConfigurationWithHibernateValidatorMissingElImplTests` | `@ClassPathExclusions({"tomcat-embed-el-*.jar", "el-api-*.jar"})` | class-level (line 35) |
| `module/spring-boot-validation` | `ValidationAutoConfigurationWithoutValidatorTests` | `@ClassPathExclusions("hibernate-validator-*.jar")` | class-level (line 34) |
| `module/spring-boot-webservices` | `...client.WebServiceMessageSenderFactoryTests` | `@ClassPathExclusions("httpclient5-*.jar")` | 1 test method (line 55) |

All 5 match this doc's core signature exactly: `.out.log` is **completely
empty** (0 lines — not even a Spring Boot banner) and `.err.log` is
overwhelmingly the same repeating `InterceptingExecutableInvoker`
`gen_heap::get_field` OOB guard warning, alternating between 1-2 object
addresses, at roughly 1-2s cadence, for the entire process lifetime (all 5
ran to the exact `TIMEOUT`/`HANG` wall time in `results.tsv`: 300.06-300.13s):

| Class | `.err.log` total lines | lines that are the repeating guard warning | `.out.log` lines |
|---|---:|---:|---:|
| `WebMvcEndpointManagementContextConfigurationTests` | 407 | 402 | 0 |
| `OAuth2AuthorizationServerAutoConfigurationTests` | 519 | 512 (hit `OOB_DIAG_CAP`) | 0 |
| `ValidationAutoConfigurationWithHibernateValidatorMissingElImplTests` | 230 | 225 | 0 |
| `ValidationAutoConfigurationWithoutValidatorTests` | 230 | 225 | 0 |
| `WebServiceMessageSenderFactoryTests` | 230 | 225 | 0 |

The two class-level-annotated `Validation*` classes show the churn starting
almost immediately (consistent with the doc's existing `Liquibase423AutoConfigurationTests`
observation: no prior test methods to "warm up" through before hitting the
annotated path). The 3 method-level-annotated classes (`WebMvcEndpointManagementContextConfigurationTests`,
`OAuth2AuthorizationServerAutoConfigurationTests`, `WebServiceMessageSenderFactoryTests`)
show the identical churn for their *entire* log too, with zero real
application content (no Tomcat/Hibernate/Spring Boot banner lines at all,
unlike this doc's `ManagementWebSecurityAutoConfigurationTests` example) —
plausibly because in these 3 classes the earlier, non-excluded test methods
either don't reach a point that prints to `System.out`, or the JUnit engine
happens to schedule the annotated method early; not confirmed either way,
consistent with this doc's standing caveat that no debugger attach has yet
captured the actual blocking point.

`OAuth2AuthorizationServerAutoConfigurationTests`'s log additionally shows
two `[CCE] enhance: defined ...$$EnhancerByCGLIB$$0` lines for its
`@Configuration` test fixtures right after VM-boot fixups and before the
warning churn starts — consistent with normal CGLIB proxy setup during
early context-class processing, not itself an anomaly.

No new evidence on the exact blocking mechanism (still no direct socket/DNS
log line, still no live debugger capture) — this update only adds
source-confirmed affected classes, it does not advance the open "confirm via
thread-dump attach" next step from the original entry or the bin5 update.

## Update 2026-07-17 (bin13 rerun triage) — 4 more classes, including the extension's own test suite

Triaged `bin13.tsv`'s HANG classes (3 in `test-support/spring-boot-test-support`,
1 in `module/spring-boot-mustache`) and grepped their source for the trigger
annotations before concluding anything from log shape alone, per this doc's
established method:

| Module | Class | Annotation (source-confirmed) | Scope |
|---|---|---|---|
| `test-support/spring-boot-test-support` | `...classpath.ModifiedClassPathExtensionExclusionsTests` | `@ClassPathExclusions(files = "hibernate-validator-*.jar", packages = "java.net.http")` | class-level |
| `test-support/spring-boot-test-support` | `...classpath.ModifiedClassPathExtensionForkTests` | `@ForkedClassPath` (sibling mechanism, same extension class) | class-level |
| `test-support/spring-boot-test-support` | `...classpath.ModifiedClassPathExtensionOverridesTests` | `@ClassPathOverrides("org.springframework:spring-context:4.1.0.RELEASE")` | class-level |
| `module/spring-boot-mustache` | `MustacheAutoConfigurationWithoutWebMvcTests` | `@ClassPathExclusions("spring-webmvc-*.jar")` | class-level |

Notably the first three ARE `ModifiedClassPathExtension`'s own JUnit test
suite (`test-support/spring-boot-test-support` is the module that *defines*
the extension) — so this cluster now blocks not just consumers of the
mechanism but the mechanism's own self-tests, meaning nothing in this
module or anywhere depending on `@ClassPathExclusions`/`@ClassPathOverrides`/
`@ForkedClassPath` can pass while this is open.

All 4 match the doc's core repeating-warning signature exactly: `.out.log`
is completely empty (0 bytes) and `.err.log` is 230-233 lines, alternating
between exactly 2 `InterceptingExecutableInvoker` object addresses at
roughly 1s cadence for the full ~68-83s captured window before the log-file
line cap cuts it off (all 4 ran to the shard's `HANG` timeout per
`bin13.tsv`). Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/test-support_spring-boot-test-support.org.springframework.boot.testsupport.classpath.ModifiedC-c70024f69d7b.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/test-support_spring-boot-test-support.org.springframework.boot.testsupport.classpath.ModifiedC-befbe0df276b.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/test-support_spring-boot-test-support.org.springframework.boot.testsupport.classpath.ModifiedC-d45d6d67b931.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-mustache.org.springframework.boot.mustache.autoconfigure.MustacheAutoConfig-38be4aa58e00.err.log`

No new evidence on the exact blocking mechanism from this batch either (no
socket/DNS log line, no debugger attach performed) — adds source-confirmed
affected classes only, same as the bin5/bin8 updates above.

## Update 2026-07-17 (bin7 rerun triage) — 4 more classes, 2 more modules

Triaged `bin7.tsv`'s HANG classes and grepped their source for the trigger
annotations before concluding anything from log shape alone, per this doc's
established method:

| Module | Class | Annotation (source-confirmed) | Scope |
|---|---|---|---|
| `module/spring-boot-data-redis` | `DataRedisAutoConfigurationJedisTests` | `@ClassPathExclusions("lettuce-core-*.jar")` | class-level |
| `module/spring-boot-data-redis` | `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests` | `@ClassPathExclusions("commons-pool2-*.jar")` | class-level |
| `module/spring-boot-data-redis` | `health.DataRedisHealthContributorAutoConfigurationTests` | `@ClassPathExclusions({"reactor-core*.jar", "lettuce-core*.jar"})` | class-level |
| `module/spring-boot-websocket` | `servlet.WebSocketMessagingAutoConfigurationTests` | `@ClassPathExclusions("jackson-*-3*")` | 2 test methods |

All 4 match this doc's core signature: `.out.log` is empty (0-3 bytes) and
`.err.log` is dominated by the repeating `InterceptingExecutableInvoker`
`gen_heap::get_field` OOB guard warning against 1-2 fixed object addresses,
at roughly a 1-5s cadence, for the entire process lifetime, killed at the
shard timeout with no `SBRUNNER_RESULT`. Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-redis.org.springframework.boot.data.redis.autoconfigure.DataRedisAutoC-7b09ad148f2a.err.log`,
`...DataRedisAutoC-77c693e1f2c7.err.log`,
`...health.DataRed-dc6ecb50c06b.err.log`,
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-websocket.org.springframework.boot.websocket.autoconfigure.servlet.WebSocke-06b735f8a994.err.log`.

No new evidence on the exact blocking mechanism beyond this doc's existing
`ConnectionFactoryUnwrapperTests` `StackOverflowError` finding (still the
strongest confirmed evidence in this doc — the recursive
`isModifiedClassPathClassLoader` guard never tripping) — this update only
adds source-confirmed affected classes.

**Cross-reference:** this same 4-class batch was also noted (independently,
before finding this doc) in
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md),
which tracks the same `InterceptingExecutableInvoker`/`InvocationInterceptorChain`
guard-warning *log signature* but also includes several HANG classes
(Tomcat, OpenTelemetry, Spring Data JPA) confirmed via source read to use
**none** of `@ClassPathExclusions`/`@ClassPathOverrides`/`@ForkedClassPath`.
That doc's population is therefore broader than this one's — this doc
(source-confirmed `ModifiedClassPathExtension` trigger on every listed
class, plus a directly-observed `StackOverflowError` proving the recursive
mechanism) is the stronger, more specific root-cause candidate for the
classes both docs share; the sibling doc's remaining non-`ModifiedClassPathExtension`
classes need their own explanation and should not be assumed to share this
doc's mechanism just because the log noise looks the same.

## Update 2026-07-19 (FIXED) — root cause was classloader-identity, not network; recursion guard fixed; 3 residuals filed separately

Picked up the in-progress fix (worktree `CratonVM-aether-modifiedclasspath-20260718-019f753a`,
branch `codex/fix-springboot-aether-modifiedclasspath-20260718-019f753a`) and
verified, extended, and shipped it. **The 2026-07-17 network-hang hypothesis
was a red herring** — the real mechanism is exactly the one this doc's
"SOE evidence points at recursive self-invocation" update already suspected:
`isModifiedClassPathClassLoader`'s guard never tripped, so every
`@ClassPathExclusions`/`@ClassPathOverrides` test recursed into a fresh
nested `Launcher.execute()` forever (StackOverflowError when fast, apparent
HANG when each recursive pass was slow enough that the shard timeout won the
race first — explaining why the same mechanism produced two different-looking
symptoms across this doc's batches).

**Confirmed root cause:** `ModifiedClassPathClassLoader` (a `URLClassLoader`
whose parent chain terminates at the bootstrap/platform loader, never
reaching the app loader — i.e. an "isolated" loader) could define its own
copy of the test class, but class references made *from* that class (its
superclass, interfaces, and any type touched by reflection) kept resolving
through CratonVM's global/flat class store instead of through the isolated
loader that defined the referencing class. `testClass.getClassLoader()` as
seen by `isModifiedClassPathClassLoader()` therefore never actually observed
the fresh `ModifiedClassPathClassLoader` identity consistently enough for the
guard to trip, and/or supertype resolution silently fell through to the app
loader's global copy — either way, `interceptMethod` kept concluding "this is
still the original classpath" and launched another nested `Launcher`.

**The fix** (native-builtins `classloader.rs`/`classloader_real.rs`/
`lang_system.rs`/`jboss_module_loader.rs`, `vm/src/runtime/interpreter.rs`):

- `url_classloader_isolated_from_app()` (existing helper, made `pub`) detects
  an isolated `URLClassLoader` by walking its parent chain.
- `ucl_try_define_local_class` / `UnsafeCoerce`'s real-JDK class-definition
  path now throws `ClassNotFoundException` from *this* loader instead of
  silently falling through to the global store when an isolated loader can't
  find the class bytes in its own (filtered) URL list — so an isolated
  loader's `loadClass` failure stays a failure instead of quietly resolving
  to the wrong namespace.
- `preload_isolated_loader_supertypes` (new, `lang_system.rs`) eagerly
  resolves a newly-defined class's direct superclass and interfaces through
  the *same* isolated loader at definition time, so the hierarchy is
  consistently loaded from one namespace instead of splitting across the
  isolated loader and the global store.
- `resolve_class_loader_aware` (interpreter.rs) gained an isolated-loader
  branch: when the defining-loader-initiated lookup (`drive_defining_loader_load`)
  can't resolve a name and the referencing class's defining loader is
  isolated, throw `NoClassDefFoundError` instead of falling through to
  `shared.load_class_concurrent` (the global store) — this is the change
  that actually fixes `isModifiedClassPathClassLoader`: the reloaded test
  class's `getClassLoader()` now consistently reports the isolated loader
  instead of intermittently resolving through the global namespace.

I additionally removed an eager `getDeclaredMethods()`-time validation
(`ensure_isolated_descriptor_types_visible` in `lang_class.rs`) that the
in-progress fix had added: it walked every declared method's parameter/return
descriptor and eagerly `loadClass()`-checked each through the isolated loader,
throwing `NoClassDefFoundError` for the whole enumeration if any one method
had an unresolvable type. This doesn't match real JVM semantics (`Class.
getDeclaredMethods()` doesn't eagerly validate reachability of every method's
parameter/return types — only actually accessing a specific `Method`'s
`getReturnType()`/`getParameterTypes()` does), and — more concretely — Spring
relies on `ReflectionUtils.getDeclaredMethods()` swallowing a *per-method*
`NoClassDefFoundError` internally and treating just that class as having zero
declared methods, which this eager, whole-class-enumeration check would have
broken for any isolated-loader class with even one optional-dependency method
signature. A standalone probe (`Class.getDeclaredMethods()` through a hand-built
isolated loader) confirmed method enumeration still works correctly for
isolated-loader classes after the removal.

**Verified fixed**, by direct testing in the worktree (`sb-runner`
harness, real classpaths, not mocks):

- `ConnectionFactoryUnwrapperTests` — this doc's directly-observed
  `StackOverflowError` (95 recursive `Launcher.execute()` passes, 7354 real
  frames) — no longer overflows; completes in ~2.8s with 11/12 tests passing
  (1 residual, filed separately, see below).
- `ModifiedClassPathExtension`'s own self-test suite —
  `ModifiedClassPathExtensionExclusionsTests` (5/5),
  `ModifiedClassPathExtensionForkTests` (1/1),
  `ModifiedClassPathExtensionOverridesTests` (2/2) — all pass 100%. These are
  the extension's own tests (`test-support/spring-boot-test-support`), so
  this also unblocks everything else in the repo that depends on
  `@ClassPathExclusions`/`@ClassPathOverrides`/`@ForkedClassPath`.
- Across a regression sweep of all classes in the "Affected classes" table
  below plus the 3 extension self-tests: every class that previously HUNG
  indefinitely (shard-timeout kill, zero `SBRUNNER_RESULT`) now completes and
  produces a real JUnit summary — most fully passing, a handful surfacing
  new (previously-masked-by-the-hang) failures now tracked as separate
  residual docs rather than blocking this fix. See in-progress sweep results
  and residual docs below; this doc is retired to `docs/internal/springboot/`
  with this fix.

**3 residuals filed separately, all since independently resolved by dev
drift** (per this repo's known-issues triage rule — fixed goes to
`docs/internal`, residuals get their own open doc so they don't block
retiring this one). Filed as OPEN against a binary built before merging
~106 commits of `origin/dev` into the fix branch; after that merge (pulling
in unrelated fixes already landed by other concurrent sessions) and a
rebuild, all 3 were re-verified passing and moved to `docs/internal` too —
kept as records of the symptoms/hypotheses rather than deleted, in case any
regress:

1. [`connectionfactoryunwrappertests-nested-outer-instance-identity-FIXED.md`](connectionfactoryunwrappertests-nested-outer-instance-identity-FIXED.md) —
   `ConnectionFactoryUnwrapperTests.Unwrap.unwrapWithoutJmsPoolOnClasspath()`
   (a `@Nested` class under a method-level `@ClassPathExclusions`) fails with
   `IllegalArgumentException: argument type mismatch` constructing the nested
   class's outer-instance reference. Narrow (only 1 of the ~29 affected
   classes in this cluster combines `@Nested` with `@ClassPathExclusions`).
2. [`isolated-loader-onbeancondition-type-deduction-bypass-FIXED.md`](isolated-loader-onbeancondition-type-deduction-bypass-FIXED.md) —
   `OnBeanConditionTypeDeductionFailureTests` expects a `NoClassDefFoundError`
   from a class-path-excluded `jackson-core` when `ObjectMapper` is
   constructed via real `@Bean` method bytecode; on CratonVM the construction
   silently succeeds instead (the excluded jar's classes remain reachable
   through *some* resolution path despite the isolated loader's URL-list
   filtering correctly excluding them, confirmed via a standalone
   `Class.forName`-based probe that *does* correctly fail — narrowing this to
   a bytecode-level, not reflection-level, gap). Root cause not fully pinned
   down; likely related to `EhCache3CacheAutoConfigurationTests`'
   `@ConditionalOnMissingBean did not specify a bean using type, name or
   annotation` failure and the Jersey/Security `ObjectProvider<X>` bean
   resolution failures below — plausibly all downstream of the same
   classloader-identity-split family as the now-fixed recursion bug, but not
   confirmed to share a single mechanism.
3. [`isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md`](isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md) —
   `JerseyChildManagementContextConfigurationTests` (5/6 methods),
   `SecurityFilterAutoConfigurationEarlyInitializationTests`,
   `ManagementWebSecurityAutoConfigurationTests`, and
   `ReactiveManagementWebSecurityAutoConfigurationTests` all fail with
   `NoSuchBeanDefinitionException: No qualifying bean of type
   'ObjectProvider<X>'` for an `X` that IS defined in the same
   `@ManagementContextConfiguration` — consistent with a classloader-identity
   split between the injection point's generic parameter type and the
   registered bean definition's type, both nominally the same class loaded
   through the isolated loader.

A full regression sweep across all ~29 classes in the "Affected classes"
table below (post dev-drift-merge binary) confirmed every class that
previously HUNG now completes; every class-level failure this doc's fix
work directly investigated (the 3 residuals above, plus
`WebMvcEndpointManagementContextConfigurationTests`,
`OAuth2AuthorizationServerAutoConfigurationTests`, and
`ValidationAutoConfigurationWithHibernateValidatorMissingElImplTests`, which
surfaced single-test failures mid-sweep before the drift merge) passed
cleanly once re-run against the post-merge binary — consistent with the
residuals above being resolved by the same unrelated upstream fixes, not
independently re-verified one-by-one for these last 3.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.aop.NonAspectJAopAutoConfigurationTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.condition.ConditionalOnCheckpointRestoreTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.condition.ConditionalOnMissingBeanWithFilteredClasspathTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.condition.OnBeanConditionTypeDeductionFailureTests` |
| `module/spring-boot-jms` | `org.springframework.boot.jms.ConnectionFactoryUnwrapperTests` (`StackOverflowError`, see Update below) |
| `module/spring-boot-gson` | `org.springframework.boot.gson.autoconfigure.Gson210AutoConfigurationTests` (HANG, see Update below) |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.web.servlet.PathRequestTests` (added bin5) |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests` (added bin5) |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests` (added bin5) |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.ReactiveManagementWebSecurityAutoConfigurationTests` (added bin5) |
| `module/spring-boot-liquibase` | `org.springframework.boot.liquibase.autoconfigure.Liquibase423AutoConfigurationTests` (added bin5) |
| `module/spring-boot-cache` | `org.springframework.boot.cache.autoconfigure.EhCache3CacheAutoConfigurationTests` (added bin5) |
| `module/spring-boot-jersey` | `org.springframework.boot.jersey.autoconfigure.actuate.web.JerseyChildManagementContextConfigurationTests` (added bin5) |
| `module/spring-boot-jersey` | `org.springframework.boot.jersey.autoconfigure.actuate.web.JerseySameManagementContextConfigurationTests` (added bin5) |
| `module/spring-boot-jersey` | `org.springframework.boot.jersey.autoconfigure.JerseyAutoConfigurationTests` (added bin5) |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.logging.log4j2.Log4J2MetricsWithLog4jLoggerContextAutoConfigurationTests` (added 2026-07-17, see "Update" above) |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.logging.logback.LogbackMetricsAutoConfigurationWithLog4j2AndLogbackTests` (added 2026-07-17, see "Update" above) |
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.actuate.web.WebMvcEndpointManagementContextConfigurationTests` (added bin8) |
| `module/spring-boot-security-oauth2-authorization-server` | `org.springframework.boot.security.oauth2.server.authorization.autoconfigure.servlet.OAuth2AuthorizationServerAutoConfigurationTests` (added bin8) |
| `module/spring-boot-validation` | `org.springframework.boot.validation.autoconfigure.ValidationAutoConfigurationWithHibernateValidatorMissingElImplTests` (added bin8) |
| `module/spring-boot-validation` | `org.springframework.boot.validation.autoconfigure.ValidationAutoConfigurationWithoutValidatorTests` (added bin8) |
| `module/spring-boot-webservices` | `org.springframework.boot.webservices.client.WebServiceMessageSenderFactoryTests` (added bin8) |
| `test-support/spring-boot-test-support` | `org.springframework.boot.testsupport.classpath.ModifiedClassPathExtensionExclusionsTests` (added bin13) |
| `test-support/spring-boot-test-support` | `org.springframework.boot.testsupport.classpath.ModifiedClassPathExtensionForkTests` (added bin13) |
| `test-support/spring-boot-test-support` | `org.springframework.boot.testsupport.classpath.ModifiedClassPathExtensionOverridesTests` (added bin13) |
| `module/spring-boot-mustache` | `org.springframework.boot.mustache.autoconfigure.MustacheAutoConfigurationWithoutWebMvcTests` (added bin13) |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationJedisTests` (added bin7) |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests` (added bin7) |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.health.DataRedisHealthContributorAutoConfigurationTests` (added bin7) |
| `module/spring-boot-websocket` | `org.springframework.boot.websocket.autoconfigure.servlet.WebSocketMessagingAutoConfigurationTests` (added bin7) |
