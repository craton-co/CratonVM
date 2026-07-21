# `WebFluxManagementChildContextConfigurationIntegrationTests`: HANG fixed; ObjectProvider `NoSuchBeanDefinitionException` fixed; DIFFERENT loader-identity residual remains OPEN

**Status: OPEN — hang fixed (confirmed gone as of `dev` a026c2c4c); the
`ObjectProvider<TomcatConnectorCustomizer>` `NoSuchBeanDefinitionException`
residual root-caused and FIXED 2026-07-20 (commit `65d738bb5`, merged
`91f514f64`); a DIFFERENT, deeper `ServerProperties` loader-identity
divergence now surfaces in its place — see the 2026-07-20 (third session)
update below. Found 2026-07-19; broader than webflux, confirmed 2026-07-20.**

## Update 2026-07-20 (third session) — ObjectProvider `NoSuchBeanDefinitionException` root-caused and FIXED (2 loader-identity bugs); deeper, DIFFERENT residual now blocking full closure

Picked up this doc's remaining single failure (`WebFluxManagementChildContextConfigurationIntegrationTests#refreshSucceedsWithoutHealth`, `AnnotationConfigReactiveWebServerApplicationContext` "Exception encountered during context initialization" from the update above). Root cause: an `ObjectProvider<TomcatConnectorCustomizer>` `@Bean` factory-method parameter with zero matching beans (by design — the whole point of `ObjectProvider`) threw `NoSuchBeanDefinitionException` instead of resolving to an empty provider, because `DefaultListableBeanFactory.resolveDependency`'s `ObjectFactory.class == descriptor.getDependencyType() || ObjectProvider.class == ...` identity fast-path never matched.

This test is the only one in the class annotated `@ClassPathExclusions` — the only one that runs under a Spring Boot `ModifiedClassPathClassLoader`-forked JUnit invocation (`ModifiedClassPathExtension`), re-running the whole test method through a nested `Launcher.discover()`+`execute()` call with `Thread.currentThread()`'s context classloader swapped to the isolated child loader. The other 4 tests in the class run under the plain application loader and pass.

**Root-caused via targeted debug tracing** (temporary env-gated `eprintln!` instrumentation added and removed across several build/trace cycles — not landed) to two independent loader-identity bugs, both following the same shape ("resolve a well-known Spring/framework class by NAME through a global, loader-blind lookup instead of anchoring the resolution to whichever loader the calling code actually belongs to"):

1. **`native-builtins/src/spring_startup_bootstrap.rs`, `GenericApplicationContext.getBeanFactory()`'s native recovery shim (`get_or_create_bean_factory`)**: when `beanFactory` is null (constructor failed before `PUTFIELD`), it built the fallback `DefaultListableBeanFactory` via `ctx.new_object(DLBF)` — a hardcoded, loader-blind global name lookup — always collapsing to the first/global (Application-loader) definer regardless of which loader the receiving `GenericApplicationContext` instance itself belongs to.
2. **`vm/src/runtime/interpreter.rs`, lambda dispatch for `InvokeSpecial`/`NewInvokeSpecial`/`GetStatic`/`PutStatic` method-handle kinds**: these "no receiver to anchor identity on" dispatch arms (e.g. `Type::new`, `Type::staticMethod`) only consulted `lambda_impl_dispatch_override`'s passive, no-re-entrant-call cache and fell straight to a global `class_manager.load_class(name)` on a miss — never actively driving the host loader's own `loadClass` for a method reference resolved for the first time under an isolated loader.

`AnnotationConfigReactiveWebServerApplicationContext::new` (a `REF_newInvokeSpecial` constructor reference in the test class's own field initializer, itself correctly reloaded fresh under the isolated loader) hit exactly this: it silently constructed the Application-loader's `GenericApplicationContext`/`DefaultListableBeanFactory` instead of the isolated loader's own copy — so *its* `ObjectProvider.class`/`ObjectFactory.class` never matched the isolated loader's copy referenced by every autoconfiguration `@Bean` parameter.

**Fixed both** (commit `65d738bb5`, merged to `dev` as `91f514f64`):
- (1) now resolves `DefaultListableBeanFactory` near the receiver's own class first (`class_id_by_name_near` + `new_object_initialized_with_class_id`), matching the pattern already used elsewhere in this file, falling back to the old global path only if that misses.
- (2) reuses `lambda_impl_dispatch_override_driven` — a function a **concurrent session added the same day** for an unrelated `SpringApplication`/`EnvironmentPostProcessorsFactory::fromSpringFactories` bootstrap symptom, via the *identical* mechanism (drive the host loader's `loadClass` on a cache miss instead of only reading a passive cache) — at the four dispatch sites that still only had the old passive-only check or no check at all.

**Verified no regressions** (Azure host, `victor@20.83.144.174:/data/data/`):
- `BinderTests`: 32/32 pass.
- `WebMvcObservationAutoConfigurationTests`: 13/13 pass.
- `ConfigurationPropertiesTests`: bisected a `loadWhenHasMultiplePropertySourcesPlaceholderConfigurerShouldLogWarning(CapturedOutput)` intermittent failure that first appeared against the combined (both fixes) binary — 5 separate runs of the combined binary gave 2 FAIL / 3 PASS, while an unmodified-`dev` baseline (2 runs) and each fix built and tested **in isolation** (1 run each) all passed cleanly (100%). This pattern — intermittent only on the combined binary, deterministic-clean on every isolated build and baseline — is consistent with this VM's known `CapturedOutput` log-timing flakiness (see the wider `captured-output-residuals` history for this codebase), not a functional regression from either fix; neither fix touches logging/output-capture machinery.

**New residual exposed by this fix, NOT fixed — doc stays OPEN**: `refreshSucceedsWithoutHealth` now fails differently — `Method.invoke` on `TomcatWebServerConfiguration.tomcatWebServerFactoryCustomizer(Environment, ServerProperties, TomcatServerProperties, WebProperties)` throws `IllegalArgumentException: argument type mismatch` because the `ServerProperties` **argument value** is an instance of the Application loader's `ServerProperties`, while the method's declared parameter type correctly resolves to the isolated loader's `ServerProperties` (confirmed via the same debug tracing: `expected_loader=7 arg_class=...ServerProperties arg_loader=2`).

Narrowed the divergence precisely by comparing `ServerProperties` against its sibling `TomcatServerProperties` (same test, same mechanism, but resolves *correctly*):
- `TomcatServerProperties` is registered via `@EnableConfigurationProperties(TomcatServerProperties.class)` declared **directly** on `TomcatReactiveWebServerAutoConfiguration` — one of the test's own top-level `AutoConfigurations.of(...)` entries — and correctly ends up on the isolated loader.
- `ServerProperties` is registered via `@EnableConfigurationProperties(ServerProperties.class)` declared on `ReactiveWebServerConfiguration` — reached only **transitively**, via `@Import({TomcatWebServerConfiguration.class, ReactiveWebServerConfiguration.class})` on `TomcatReactiveWebServerAutoConfiguration` — and ends up on the WRONG (Application) loader, even though `ReactiveWebServerConfiguration`'s own class correctly resolves to the isolated loader (confirmed in the trace).

**Working hypothesis (not confirmed, not fixed)**: `ConfigurationClassParser` reads a transitively-`@Import`ed configuration class's own annotations (`@EnableConfigurationProperties` here) via ASM-based `MetadataReader`/`AnnotationMetadata` — a different code path from the direct-reflection annotation resolution used for top-level `AutoConfigurations.of(...)` entries (that path already threads loader identity correctly, per the `container_loader`/`resolve_annotation_class_via_loader` mechanism and the Enum-arm fix landed earlier the same day in commit `22deca366`). If CratonVM's ASM/`MetadataReader` integration resolves a Class-valued annotation attribute's name without threading the same loader-identity context, that would explain exactly this top-level-vs-transitive split.

**Cross-reference**: [`observationregistry-conditionalonmissingbean-classpathexclusions.md`](observationregistry-conditionalonmissingbean-classpathexclusions.md) — an independent session hit the *same underlying class of bug* the same day, via a different symptom (`@ConditionalOnMissingBean` failing to recognize an existing user-registered bean under `@ClassPathExclusions`), and explicitly floated the identical hypothesis ("`@ConditionalOnMissingBean` ... reads the `@Bean` method's return type via ASM bytecode parsing ... a completely different code path from `ClassLoader.loadClass`"). Whoever picks up either doc next should treat them as the same root cause until proven otherwise — start by locating CratonVM's `MetadataReader`/ASM annotation-attribute Class-resolution code (not yet located this session) and auditing whether it threads loader identity the way `resolve_annotation_class_via_loader`/`container_loader` already do for direct reflection.

**Next steps for whoever picks this up:**
1. Find CratonVM's ASM/`MetadataReader`-backed annotation-attribute Class-value resolution (likely wherever `org.springframework.core.type.classreading.*` gets special native handling, or wherever real ASM bytecode parsing of `.class` files is supported) and check whether it resolves Class-valued attributes (`@EnableConfigurationProperties(X.class)`, `@ConditionalOnMissingBean(X.class)`, etc.) relative to the correct defining loader for classes reached only via nested `@Import`/transitive processing.
2. Re-run `refreshSucceedsWithoutHealth` after any fix there; if it passes, re-run the full 5-class set from this doc plus `observationregistry-conditionalonmissingbean-classpathexclusions.md`'s 2 classes to check for a shared fix.
3. Only retire this doc to `docs/internal/` once `refreshSucceedsWithoutHealth` (and this doc's other listed classes) pass with zero remaining failures — per the `docs/known-issues` convention, a doc with ANY open sub-part stays in `known-issues/`.

## Update 2026-07-20 (later same day) — hang no longer reproduces at dev tip `a026c2c4c`; not bisected, not this session's fix

Re-ran all 5 classes below (the original plus the 4 found earlier today)
against a `dev`-tip build merged with an unrelated `origin/dev` fast-forward
(`54003fb83` → `a026c2c4c`, ~dozens of concurrent commits from other
sessions in between — this session did not touch anything in the
classloader/annotation-scanning area). All 4 of today's HANGs now complete:
`ConfigurationPropertiesTests` 114/114 PASS (39s), `BinderTests` PASS (4s),
`WebMvcObservationAutoConfigurationTests` PASS (21s). The **original**
`WebFluxManagementChildContextConfigurationIntegrationTests` also no longer
hangs, but now surfaces a **different, real failure** instead (1 of 5
tests, `AnnotationConfigReactiveWebServerApplicationContext` logs "Exception
encountered during context initialization" — not yet triaged). Not bisected
to the specific fixing commit (out of scope this session — found this
entirely as a side effect of re-verifying an unrelated fix's merge); given
the sheer commit volume in that window this is a "some concurrent session
fixed it" situation, not a deliberate fix. `SpringApplicationTests` (listed
below as "possibly the same slowdown") is a red herring — it still fails
(4/102, down from 12/102 on the pre-fix baseline) but was never actually a
hang, and its failures don't match this doc's classloader-hang signature at
all; drop it from this doc's scope.

Keeping this doc OPEN rather than archiving: the hang mechanism itself was
never root-caused (this update proves it's now gone, not why), and the
newly-exposed `WebFluxManagementChildContextConfigurationIntegrationTests`
failure needs its own triage. Whoever picks this up next should start from
re-confirming this doc's remaining single failure rather than the hang.

## Update 2026-07-20 — 4 more affected classes, CPU-sampled (busy, not parked), pre-existing on unmodified `dev`

Found while fixing
[`capturedoutput-empty-console-cluster.md`](capturedoutput-empty-console-cluster.md)
(archived — see
`../../internal/springboot/capturedoutput-empty-console-cluster-FIXED.md`):
running `core/spring-boot`'s `ConfigurationPropertiesTests` (114 tests) as a
full class — not just its one CapturedOutput-affected method — hangs the
same way, and 3 more classes in unrelated modules do too:
`core/spring-boot`'s `org.springframework.boot.context.properties.bind.BinderTests`,
`module/spring-boot-webmvc`'s
`org.springframework.boot.webmvc.autoconfigure.WebMvcObservationAutoConfigurationTests`
(HANG as a full class, despite its 2 individually-listed CapturedOutput
methods now passing — see the archived doc above), and `core/spring-boot`'s
`org.springframework.boot.SpringApplicationTests` (this one completes but
takes 150s+ for 102 tests, vs. a few seconds for comparable classes —
possibly the same underlying slowdown without fully deadlocking).

**Confirmed pre-existing on unmodified `dev`** (not a regression from the
`capturedoutput` fix): re-ran all 3 hanging classes against a `dev`-tip
binary built *before* any of that session's changes
(`cratonvm-dev-baseline-check.exe`, worktree main `C:\craton\CratonVM` at
`54003fb83`) — identical HANG/slow-FAIL results, byte-for-byte the same
shape. `ConfigurationPropertiesTests`' `err.log` tail matches this doc's
existing symptom exactly: `...DEBUG [org.hibernate.validator.internal.xml
.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via
Hibernate Validator's class loader` then nothing further — same last line,
same silence.

**New evidence: CPU-sampled busy, not parked** (this doc's own "next steps"
asked for this). Two `Get-Process -Name <exe> | .CPU` samples ~10s apart on
the hung `ConfigurationPropertiesTests` process showed CPU time climbing
(18.0s → 31.0s) — the process is doing real work, not blocked on a lock or
I/O. `CRATONVM_DBG_HANG_SAMPLE=1` (existing sampling probe,
`interpreter.rs:18738`, prints every 200,000th `execute_invoke_kind` call)
shows progress reaches call #200,000 quickly (within seconds) but **never
reaches call #400,000** even after a full 300s window — both with JIT on
and with `--nojit`. The specific method sampled at #200,000 varies between
runs (`org.springframework.core.annotation.TypeMappedAnnotations.scan`,
`java/lang/Thread.getContextClassLoader`, `java/lang/Object.equals`) —
consistent with a large but apparently-nonterminating amount of reflective
work (Spring's meta-annotation scanning / classloader delegation), not one
single fixed infinite loop always hit at the same call. Whether this is a
genuine infinite loop (e.g., a broken cycle-detection guard in annotation
meta-scanning under CratonVM's `Class`/annotation mirror equality) or an
extreme, unbounded slowdown was not determined — the process never produced
a 3rd `HANG_SAMPLE` line even at a 1200s (20-minute) timeout in one run,
which favors "genuinely stuck," but this is not conclusively proven.

Not bisected further this session (out of scope — found while chasing an
unrelated captured-output bug). Whoever picks up this doc's existing "next
steps" (bisect the ~122-commit window, get a real stack dump) should
prioritize it given it now spans at least 5 classes across 3 modules, not
just the original 1.

---

**Status (original, 2026-07-19): OPEN**

## Symptom

`org.springframework.boot.webflux.autoconfigure.actuate.web
.WebFluxManagementChildContextConfigurationIntegrationTests` hangs (no
JUnit summary, process killed on suite-runner timeout). Progress is real
but stops permanently at a consistent point:

```
INFO org.springframework.boot.tomcat.TomcatWebServer -- Tomcat initialized with port 0 (http)
...
INFO [org.apache.coyote.http11.Http11NioProtocol] Initializing ProtocolHandler ["http-nio-auto-1"]
INFO [org.apache.catalina.core.StandardService] Starting service [Tomcat]
INFO [org.apache.catalina.core.StandardEngine] Starting Servlet engine: [Apache Tomcat/11.0.22]
WARN [org.apache.catalina.util.SessionIdGeneratorBase] The default SHA1PRNG algorithm for SecureRandom is not supported by this JVM. Using the platform default.
INFO [org.hibernate.validator.internal.util.Version] HV000001: Hibernate Validator 9.1.0.Final
DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL
DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via user class loader
DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via TCCL
DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via Hibernate Validator's class loader
```

...then nothing further — no exception, no next debug line, no GC/thread
activity resembling progress. Timeout kills the process (300s+, tested up
to 420s in one run) with 0 tests reported.

## Discovery context (not this doc's original bug)

Found while re-verifying
[`../../internal/springboot/spring-boot-webflux-residuals-FIXED.md`](../../internal/springboot/spring-boot-webflux-residuals-FIXED.md)'s
Issue B (a *different*, now-fixed hang in the same class — that one
stalled much earlier, during JUnit discovery, before Tomcat ever started,
and is documented separately as fixed). This is a distinct hang the same
test class now hits *later* in its run, discovered only after merging 122
new `origin/dev` commits into that fix's feature branch.

## Root cause: confirmed NOT related to the webflux-residuals fix; NOT bisected further

**Confirmed via isolated testing** (2026-07-19, this session):

- The webflux-residuals fix (getfield/putfield loader-aware field
  resolution + `MergedAnnotation$Adapt.isIn` identity bridge), tested
  against its own pre-merge `dev` base (`40678d0f5`), does **not** exhibit
  this hang — 3 clean full completions (see the FIXED doc above).
- After merging 122 new `origin/dev` commits into that same branch, this
  NEW hang appeared (different stall point, later in the test).
- **A from-scratch worktree built at pure `origin/dev` tip (commit
  `88fa839cc`), with NONE of the webflux-residuals branch's changes
  present at all**, reproduces the IDENTICAL hang at the IDENTICAL stall
  point (byte-for-byte matching debug-log tail). Confirmed under a
  verified-low-host-load window (39-44GB free RAM, single-digit concurrent
  build processes on this shared box, ruling out resource contention as
  the cause).

This conclusively means: **some commit in the ~122-commit window between
`40678d0f5` and `origin/dev`'s 2026-07-19 tip introduced this hang**,
independent of and unrelated to the webflux-residuals fix. Not bisected to
a specific commit — out of scope for the session that found it. Given the
stall is inside a `ClassLoader` resource-lookup delegation chain
(`ResourceLoaderHelper` walking user classloader → TCCL → Hibernate
Validator's own classloader), the most likely candidates are one of the
classloader-related fixes that landed in that window, e.g.:

- `URLClassLoader.getResourceAsStream` dead-in-real-JDK-mode fix
- `ClassUtils.forName` null-explicit-classloader fix
- The "isolated-loader-*" cluster (`ObjectProvider` generic identity,
  `OnBeanCondition` type deduction, `stop isolated URLClassLoader class
  resolution from silently falling through to the global classpath`)

None of these have been individually tested against this repro; this is a
plausible-candidates list, not a confirmed mechanism.

## Next steps for whoever picks this up

1. Bisect the ~122-commit window (`git log --oneline <40678d0f5>..origin/dev`)
   against this exact repro (single-class, `-Parallel 1`, `-TimeoutSec 300+`,
   verified-low host load) to find the introducing commit.
2. Once found, get a stack/thread dump on timeout (`--stack-dump-on-timeout`
   or equivalent, per this repo's crash-debug tooling) to see exactly which
   native call or lock the `ResourceLoaderHelper` classloader-resource
   lookup is blocked in — the log gives the last debug line before the
   stall, not the blocking frame itself.
3. Check whether other Hibernate-Validator-bootstrapping test classes
   (anything constructing a `jakarta.validation.Validator` under a
   non-trivial classloader hierarchy) hit the same wall — this may be a
   broader-than-webflux regression.

## Affected classes

**Hang no longer reproduces as of `dev` a026c2c4c** (see the later
2026-07-20 update above) for all 4 rows below — kept here for history /
in case it regresses again, not because they're still hanging today.

| Module | Class |
|---|---|
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.autoconfigure.actuate.web.WebFluxManagementChildContextConfigurationIntegrationTests` (hang gone; now a different, untriaged 1/5 test failure — see update above) |
| `core/spring-boot` | `org.springframework.boot.context.properties.ConfigurationPropertiesTests` (full class; found 2026-07-20, now 114/114 PASS) |
| `core/spring-boot` | `org.springframework.boot.context.properties.bind.BinderTests` (found 2026-07-20, now PASS) |
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.WebMvcObservationAutoConfigurationTests` (full class; found 2026-07-20, now PASS) |
