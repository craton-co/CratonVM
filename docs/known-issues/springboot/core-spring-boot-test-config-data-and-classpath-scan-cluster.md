# core/spring-boot-test — config-data loading gaps, duplicate classpath scan results, missing PropertySource

**Status: PARTIALLY FIXED (2026-07-20) — Clusters A, B, D, E confirmed fixed upstream (verified against dev tip, HotSpot-matching PASS); Cluster C's originally-documented NPE is now FIXED (root-caused, third investigation session), plus 2 further residuals discovered along the way are also FIXED. `SpringBootContextLoaderAotTests` still does not fully pass: after merging forward with the latest dev, it now hits a fifth, unrelated-bug-family residual (Groovy `ClassInfo`/`ReflectionCache`, see Cluster C "Residual 5") before ever reaching the fourth (`AotApplicationContextInitializer` loader identity, "Residual 4") — neither is yet root-caused.**

Originally six `core/spring-boot-test` classes failing/hanging via 5 distinct signatures, found 2026-07-17 and none root-caused at the time. Re-verified 2026-07-20 in worktree `fix/sb-configdata-classpath-scan-cluster-20260719` (branched from dev `54003fb83`, merged forward to dev `91ee66ca7`): the doc was stale — dev had already fixed Clusters A, B, D, and E independently since 2026-07-17. Only Cluster C's original signature was also stale (already-changed failure) and needed fresh investigation, which found two genuine CratonVM interpreter/native bugs (now fixed) plus one further, deeper residual (still open).

## Cluster A — classpath-root `application.properties` config-data not composed into Environment — FIXED (upstream, before this session)

| Class | Failure (as originally documented 2026-07-17) |
|---|---|
| `ConfigDataApplicationContextInitializerTests` | `expected: "bucket" but was: null` |
| `ConfigDataApplicationContextInitializerWithLegacySwitchTests` | same shape |

**Verified 2026-07-20: both PASS** against current dev (`environment.getProperty("foo")` correctly returns `"bucket"`). Not re-root-caused — some other, unrelated dev commit(s) between 2026-07-17 and 2026-07-20 fixed the classpath-root config-data composition path. No action needed.

## Cluster B — `@Value` placeholder left unresolved — FIXED (upstream, before this session)

`SpringBootTestCustomConfigNameTests`: originally `expected: "bar" but was: "${test.foo}"`.

**Verified 2026-07-20: PASSES** against current dev. Not re-root-caused. No action needed.

## Cluster C — `SpringBootContextLoaderAotTests` — originally-documented NPE FIXED (3rd session) + 2 more residuals fixed along the way; 4th, distinct residual now OPEN

The original 2026-07-17 signature (`IllegalStateException: Found multiple @SpringBootConfiguration annotated classes` via `AnnotatedClassFinder.scanPackage`) **no longer reproduces** — it was also stale. Re-running against current dev surfaces a completely different failure chain, root-caused to two distinct CratonVM interpreter/native bugs (both now fixed on branch `fix/sb-configdata-classpath-scan-cluster-20260719`, merge pending) plus a third, deeper residual.

Both fixed bugs are instances of the same general class of problem: **`SpringBootContextLoaderAotTests` is annotated `@CompileWithForkedClassLoader`**, which re-runs the entire test method inside a second, isolated `ClassLoader` (`CompileWithForkedClassLoaderClassLoader`) so AOT-generated sources can be freshly compiled and loaded. CratonVM's various "loader-aware resolution" mechanisms (built up incrementally across several `@CompileWithForkedClassLoader`/`@ClassPathOverrides` bug fixes referenced elsewhere in `docs/internal/spring/CRATONVM-SPRING-GENUINE-BUGLIST.md`) don't uniformly cover every place a `Class` identity crosses a loader boundary.

### Bug 1 (FIXED): interface-default-method dispatch bypassed a receiver's own override

`vm/src/runtime/interpreter.rs`, the `loader_interface_override` block (~line 20000, inside `execute_invoke_kind`): when an `invokeinterface` call's receiver's defining loader has its *own* copy of the CP-resolved interface, dispatch was unconditionally redirected to resolve against *that* interface's class_id — even when the receiver's own class hierarchy already declares a concrete (non-default) override of the exact method. That skipped straight past the override to the interface's own default-method body.

Concretely: `TestContextAotGenerator` calls `AotContextLoader.loadContextForAotProcessing(MergedContextConfiguration, RuntimeHints)` on a `SpringBootContextLoader` instance (loaded by the fork's classloader). `SpringBootContextLoader` overrides that exact 2-arg method (`SpringBootContextLoader.java:118-121`) but not the interface's legacy 1-arg default. Dispatch landed on `AotContextLoader`'s own 2-arg default body instead (`AotContextLoader.java`'s default just calls the 1-arg default, which unconditionally throws `UnsupportedOperationException("Invoke loadContextForAotProcessing(MergedContextConfiguration, RuntimeHints) instead")`).

**Fix**: before applying the loader-exact interface redirect, `find_method_recursive` is now run against the receiver's own `class_id` first; the redirect only fires when that resolves to an interface (i.e. the receiver would fall through to the default anyway). See the commit for the full comment/rationale.

### Bug 2 (FIXED): annotation-default `Enum` values resolved via a loader-blind global lookup

`native-builtins/src/lang_class.rs`, `annotation_element_to_java_typed`'s `AnnotationElementValue::Enum` branch: unlike its sibling `Class`-valued branch (which already resolves through `container_loader` — the annotation's declaring class's own loader — when present), the `Enum` branch always used the loader-blind global `ctx.class_id_by_name(class_name)`.

Concretely: `@SpringBootTest`'s `useMainMethod()` attribute defaults to `UseMainMethod.NEVER`. Under the fork, CratonVM's default-value resolution fetched the enum constant via the *global* table, which returned a same-named but reference-*unequal* `NEVER` constant relative to the fork-loader's own copy that `SpringBootContextLoader.getMainMethod`'s bytecode compares against with `==`. The comparison silently failed, `useMainMethod` behaved as `ALWAYS` (enum ordinal 0), and `getMainMethod` threw `IllegalStateException: Main method not found on 'SpringBootContextLoaderAotTests$ExampleConfig'`.

**Fix**: the `Enum` branch now mirrors the `Class`-valued branch — resolves via `resolve_annotation_class_via_loader(ctx, container_loader, class_name)` first, falling back to the global lookup only when no `container_loader` is available or the loader-scoped resolution fails.

### Residual (FIXED — third investigation session, 2026-07-20): `SpringFactoriesEnvironmentPostProcessorsFactory` ends up defined by a different loader than sibling SPI classes it instantiates

After both fixes above, `SpringBootContextLoaderAotTests` progresses substantially further into `SpringApplication`'s real bootstrap, then fails with:

```
java.lang.NullPointerException: Cannot invoke "org.springframework.boot.logging.DeferredLogFactory.getLog(java.lang.Class)" because "logFactory" is null
	at org.springframework.boot.cloud.CloudFoundryVcapEnvironmentPostProcessor.<init>(CloudFoundryVcapEnvironmentPostProcessor.java:106)
```

via `SpringFactoriesLoader$FactoryInstantiator.instantiate` → `resolveArgs` → `SpringFactoriesLoader.ArgumentResolver` (real, unmodified spring-core-7.0.7 bytecode). `ArgumentResolver.of(DeferredLogFactory.class, logFactory)` matches constructor parameter types by `Class.equals` (== identity, `Class` doesn't override `equals`). Added temporary tracing (removed before commit; reproducible by re-adding a debug print at the same two call sites) showed:

- `SpringFactoriesEnvironmentPostProcessorsFactory` (the class holding the `ldc DeferredLogFactory.class` literal via `ArgumentResolver.of`) is defined by the **Application** (system) loader.
- `CloudFoundryVcapEnvironmentPostProcessor` (an `EnvironmentPostProcessor` SPI implementation, discovered and instantiated via the same `SpringFactoriesLoader` call) is defined by **`UserDefined(3)`** — the fork's own loader.

These are two *different* `DeferredLogFactory` class identities, so `Class.equals` fails and the constructor argument resolves to `null`.

Per `CompileWithForkedClassLoaderClassLoader`'s real bytecode (`org/springframework/core/test/tools/CompileWithForkedClassLoaderClassLoader`, decompiled from `spring-core-test-7.0.7.jar`), its constructor sets `super(testClassLoader.getParent())` — i.e. its OWN parent is the *grandparent* of the original test class's loader, not the test loader itself — and `loadClass` delegates everything except `org.junit`/`org.testng` straight to `super.loadClass()` (standard parent-first `ClassLoader` delegation, falling to the fork's own `findClass` — which loads from the freshly-compiled-in-memory AOT sources — only when the parent chain can't resolve it). Under this scheme, for a flat single-classloader launch (which is how `SbRunner`/this suite runs, with `-cp` and no Gradle-worker-style loader layering), *both* `SpringFactoriesEnvironmentPostProcessorsFactory` and `CloudFoundryVcapEnvironmentPostProcessor` should end up defined by the fork loader (since the grandparent-only chain can't resolve either from the ordinary classpath) — so `SpringFactoriesEnvironmentPostProcessorsFactory` showing up as Application-loader-defined instead looks like the actual bug: it (or whatever earlier call resolved/instantiated it) took a shortcut back to an already-loaded, stale Application-loader copy instead of going through this classloader's real (parent-then-self) delegation.

**Not yet root-caused to a specific file:line.** Next step: trace backward from wherever `new SpringFactoriesEnvironmentPostProcessorsFactory(...)` (in real `EnvironmentPostProcessorsFactory.fromSpringFactories`, spring-boot core) executes — i.e. find the `New`/class-resolution call that decided `SpringFactoriesEnvironmentPostProcessorsFactory`'s class_id, and check whether it went through the loader-faithful path or a name-collapsed one. Likely candidates: whatever mechanism populates `crate::classloader::defining_loader_for`/the per-loader exact-class index for a class loaded via a bare (non-`URLClassLoader`) `ClassLoader.loadClass()`/`defineClass()` call chain like this one, versus the more commonly-exercised `URLClassLoader`-based forks (`ModifiedClassPathClassLoader`, used by `@ClassPathOverrides`/`@ForkedClassPath`).

Log: `apps/spring-boot-suite-runner/.suite/results/configdata-classpath-scan-postmerge/all-jit/logs/core_spring-boot-test.org.springframework.boot.test.context.SpringBootContextLoaderAotTests.out.log`.

**Update 2026-07-20 (second, independent investigation session): traced further, one more real fix landed, but the residual survives it.**

Found and fixed a genuine, related bug in `native-builtins/src/classloader.rs`'s
`builtin_loader_reachable` (consulted by `cl_real_load_class_base`'s
`scoped_user_chain`/`defer_to_find_class` gating, the exact mechanism this
doc's "Next step" above points at): it treated *any* built-in loader —
including the **platform** loader, which can only see JDK platform modules —
as "safe to consult CratonVM's flat global class store." Only the
**application**-tier loader actually is. `CompileWithForkedClassLoaderClassLoader`'s
declared parent (`testClassLoader.getParent()`) is the platform loader in a
flat `-cp` launch, so this wrongly let the flat store answer for
`EnvironmentPostProcessorsFactory` before `findClass` ever got a chance,
exactly matching this doc's own hypothesis above. Fixed by excluding the
platform loader specifically (reusing the existing `is_platform_class_loader`
helper) — a small, low-risk, JVMS-5.3-correct change with only 2 call sites
(the real-JDK and synthetic-JDK counterparts of the same gate).

**But the NPE still reproduces after this fix.** `CRATONVM_DBG_CL_SCOPE`
tracing (a temporary env-gated `eprintln!`, since removed) confirmed the fix
changes the decision as expected — `defer_to_find_class=true`,
`scoped_user_chain=true` for the `EnvironmentPostProcessorsFactory` lookup,
i.e. the flat-store step 1 is now correctly skipped and step 2 (`findClass`,
the receiver's own override) is the authoritative path. Yet
`SpringFactoriesEnvironmentPostProcessorsFactory`'s own defining loader is
*still* reported as the Application loader afterward (re-traced with the
same Java-level debug prints described above). Two possibilities, neither
confirmed:

1. `findClass()`'s own definition path (the forked loader's
   `defineDynamicClass` → `ClassLoader.defineClass` native, backing its
   `classResourceLookup` fallback that reads original `.class` bytes off the
   real classpath for non-AOT-generated classes) may not actually produce a
   loader-isolated duplicate under CratonVM's `defineClass` — i.e. it may be
   **collapsing by binary name** into the pre-existing global registration
   instead of minting a second, independent `ClassId` scoped to the new
   defining loader. This would be the flat-global-class-store architecture's
   most fundamental manifestation of this whole bug family — the same family
   that already needed three separate, narrowly-scoped patches at different
   call sites (`loader_interface_override` in `interpreter.rs`, the
   `container_loader` threading in `lang_class.rs`'s annotation-enum arm, and
   now `builtin_loader_reachable`) — suggesting the flat-store model may need
   a more systemic fix rather than continuing to patch individual call sites
   as they surface.
2. There may be a *second*, not-yet-traced resolution path (distinct from
   both `resolve_class_loader_aware`'s `New`-instruction path and
   `cl_real_load_class_base`'s `loadClass` path) through which
   `SpringFactoriesEnvironmentPostProcessorsFactory` specifically gets
   resolved, that this investigation didn't reach.

Next step for whoever picks this up: trace CratonVM's real-JDK-mode
`ClassLoader.defineClass` native implementation directly (not `loadClass`)
to check whether it deduplicates/collapses by binary name across distinct
defining-loader instances.

### Actual root cause + fix (third investigation session, 2026-07-20)

Hypothesis 2 from the second session ("a second, not-yet-traced resolution
path") was the correct one. `EnvironmentPostProcessorApplicationListener`
(a real Spring Boot class, correctly fork-loader-defined) constructs its
`postProcessorsFactory` field via the **static method reference**
`EnvironmentPostProcessorsFactory::fromSpringFactories` in its constructor —
compiled by javac into an `invokedynamic`/`LambdaMetafactory` call site, not
a plain `invokestatic`. Calling `.apply(classLoader)` on that field later
dispatches through `try_lambda_dispatch`'s `MethodHandleKind::InvokeStatic`
arm in `vm/src/runtime/interpreter.rs`, which resolves the impl method's
*owner* class via `lambda_impl_dispatch_override` — and that function only
ever *reads* an already-populated cache
(`lookup_loader_initiated`/`initiating_resolution_cache`); it never *drives*
the host loader's own `loadClass()` on a cold miss. Because this was the
*very first* touch of `EnvironmentPostProcessorsFactory` from the fork
loader's namespace — no `New`/`checkcast`/`invokestatic` had resolved it yet
to populate that cache — the miss fell through to the loader-blind
`invoke_shared(class_name, ...)`, which resolved to whichever same-named
class the flat global store already held (the Application loader's copy,
loaded earlier by some other, non-forked part of the same process). The
lambda's impl method (`fromSpringFactories`, and everything it does,
including `new SpringFactoriesEnvironmentPostProcessorsFactory(...)`) then
ran entirely under the Application loader's world, producing the
Application-loader-owned `SpringFactoriesEnvironmentPostProcessorsFactory`
this doc originally reported — while `CloudFoundryVcapEnvironmentPostProcessor`
(discovered via `Class.forName(name, false, explicitLoader)` from inside
`SpringFactoriesLoader`, an *explicit*-loader call that already worked
correctly) ended up fork-loader-owned, producing the `Class.equals` mismatch
and the `DeferredLogFactory` NPE.

**Fix** (`vm/src/runtime/interpreter.rs`): added
`lambda_impl_dispatch_override_driven`, used only at the
`MethodHandleKind::InvokeStatic` dispatch site in `try_lambda_dispatch`. It
tries the existing passive `lambda_impl_dispatch_override` first (unchanged,
still used everywhere else); on a miss, if the lambda's host class is
user-defined-loader-owned, it *actively drives* that loader's `loadClass`
(the same `drive_defining_loader_load` the `New`/`Ldc` resolution path's
gate-on branch already uses) before ever falling back to the loader-blind
global lookup. Every other lambda dispatch call site
(`InvokeVirtual`/`InvokeInterface`, the `lambda_private_impl_dispatch_class`
default-method rescue, the reflective natives) is untouched — this is
additive only at the one call site that was provably missing it.

**Verified**: `SpringBootContextLoaderAotTests` no longer throws the
`DeferredLogFactory` NPE; `SpringFactoriesEnvironmentPostProcessorsFactory`
and `CloudFoundryVcapEnvironmentPostProcessor` are now both fork-loader-owned.

### Residual 2 (FIXED, same session): lambda `checkcast` against a functional interface implemented by a *different* loader's copy of a same-named ancestor interface

Fixing the residual above let the test progress into a `ClassCastException:
? cannot be cast to org.springframework.context.ApplicationContextInitializer`
at `SpringApplication.applyInitializers` (`checkcast`, `SpringApplication.java:617`).
The `?` is CratonVM's own "no class-store entry" placeholder — the value being
cast is a lambda proxy (`class_id >= 0x8000_0000`).

`Instruction::Checkcast`'s `lambda_proxy_satisfies` (`interpreter.rs`) already
compares the lambda's own functional-interface *name* directly against the
checkcast target's name (loader-agnostic, correct), then falls back to
resolving the functional interface globally by name and checking
`is_subclass_of(iface_id, target_class_id)` — a plain `ClassId` walk. That
fails whenever the globally-resolved copy of the lambda's functional
interface (here `org.springframework.context.aot.AotApplicationContextInitializer`,
which itself `extends ApplicationContextInitializer<C>`) was first loaded
under a *different* loader than the one that defined the checkcast's target
`ApplicationContextInitializer` — both are genuinely "`ApplicationContextInitializer`"
by name, just different per-loader `Class` objects, so the `ClassId`-based
ancestor walk misses it.

**Fix**: `lambda_proxy_satisfies`'s fallback now also tries
`loader_aware_name_assignable` (the same name-based hierarchy walk
`checkcast` already uses for non-lambda receivers) before giving up.

### Residual 3 (FIXED, same session): `Class.getGenericInterfaces()` on a lambda proxy returns a raw `Class`, never a real `ParameterizedType`

Fixing residual 2 progressed the test into `IllegalStateException: No generic
type found for initializer of type class
SpringBootContextLoader$ContextLoaderHook$1$$Lambda/...` —
`GenericTypeResolver.resolveTypeArgument` (real, unmodified Spring bytecode)
requires an actual `java.lang.reflect.ParameterizedType` to extract a type
argument and throws when only a raw `Class` is available.
`native_class_get_generic_interfaces` (`native-builtins/src/lang_class.rs`)
already special-cases lambda proxies (they carry no `Signature` attribute of
their own, being synthetic), but only ever returned the raw functional-interface
`Class` mirror — never attempted to reconstruct the concrete parameterization
a real HotSpot-generated lambda class's `Signature` attribute would carry.

**Fix**: added `lambda_functional_interface_generic_type`
(`native-builtins/src/generics.rs`), which reconstructs a real, parameterized
`TypeSig::Class` for a lambda's functional interface from three pieces
CratonVM already records at `LambdaMetafactory` bootstrap time: the
interface's own `Signature` attribute (its type parameters), a **bounded walk
up its `extends` chain** to find the SAM method's actual declaring level
(the SAM is very often declared several interfaces up — e.g. Spring's
`AotApplicationContextInitializer<C> extends ApplicationContextInitializer<C>`
merely narrows/forwards `C`, never redeclaring `initialize`), threading a
type-variable substitution map through each level, and finally the lambda's
call-site `instantiated_descriptor` (the concrete, generics-free descriptor)
to resolve each root type parameter to a concrete type. Falls back to the
prior raw-`Class` behavior whenever the interface isn't generic, the SAM
can't be found within a bounded depth, or a type variable can't be matched —
strictly additive, matching the fix philosophy used everywhere else in this
loader-aware-resolution bug family.

New `NativeContext` trait method: `lambda_call_site_descriptors` (returns the
SAM method name/erased descriptor + instantiated descriptor for a lambda
proxy `ClassId`, implemented in `vm/src/vm/vm_exec.rs`).

### Residual 4 (OPEN, newly discovered): `AotApplicationContextInitializer` itself ends up Application-loader-owned

With residuals 1–3 fixed, `SpringBootContextLoaderAotTests` progresses
further still (`SpringApplication.applyInitializers` → `resolveTypeArgument`
→ `ResolvableType.as(ApplicationContextInitializer.class)`'s hierarchy walk)
but **still fails with the identical `IllegalStateException: No generic type
found...`** message. Tracing (`CRATONVM_DBG_LAMBDA_GENERIC=1`) shows the fix
above now correctly builds `AotApplicationContextInitializer<ConfigurableApplicationContext>`
for the lambda's own `getGenericInterfaces()` — but when Spring's
`ResolvableType.as()` recurses one level further and calls
`AotApplicationContextInitializer.class.getGenericInterfaces()` directly
(a *real*, non-lambda class, `class_id` observed as `4633` in one run), the
embedded `ApplicationContextInitializer` reference it builds resolves (via
`class_id_in_generic_scope`/`class_id_by_name_near` in
`native-builtins/src/generics.rs`) to the **Application** loader's copy
(observed as `ClassId(2556)`, same as the loader-blind global lookup) instead
of the fork's own copy (observed as `ClassId(1961)` — the same one
`SpringApplication`'s own bytecode correctly resolves elsewhere). Since
`Class` doesn't override `equals`, `ResolvableType.as()`'s `resolved == type`
reference check correctly (per real JVM semantics) reports them as different,
and the walk bottoms out at `Object` with no match.

This is the *fourth*, structurally distinct instance of the same root
pattern this whole cluster keeps surfacing (a class gets defined by the
Application loader when it should be fork-loader-owned, because something
resolved/touched it via a loader-blind path before the fork's own machinery
got a chance) — but for `AotApplicationContextInitializer` specifically, NOT
yet traced to the specific call site that resolves it prematurely. Given the
pattern, the leading candidates are the same families already fixed twice in
this cluster: another static-method-reference lambda (note
`AotApplicationContextInitializer` has its own
`InvokeDynamic initialize:([Ljava/lang/String;)AotApplicationContextInitializer`
factory bootstrap in its own class file) or a `Class.forName`/reflective
probe from Spring's AOT code generator (`TestContextAotGenerator`/
`ApplicationContextAotGenerator`) running during the earlier
`loadContextForAotProcessing` phase, outside the fork's own execution
context.

**Not yet root-caused to a specific file:line.** Next step for whoever picks
this up: repeat the same tracing technique used for residuals 1–3 in this
doc — add a `CRATONVM_DBG_*`-gated `eprintln!` at
`resolve_class_loader_aware`/`drive_defining_loader_load`/
`execute_invokestatic`'s `static_dispatch_class_id` computation, filtered to
`AotApplicationContextInitializer`, and reproduce via
`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1 -ClassList
onlyone.tsv` (module `core/spring-boot-test`, class
`SpringBootContextLoaderAotTests`) to find exactly which resolution first
mints its Application-loader-owned `ClassId`.

### Residual 5 (OPEN, distinct bug family, found post-merge): Groovy `ClassInfo.getClassInfo(Class)` returns `null`, unrelated to loader identity

Before merging this branch forward, Residual 4 above (traced against dev
`fba145c60`, this branch's original base) was the observed failure. After
merging the latest `origin/dev` — which independently landed an unrelated,
concurrent fix to `native-builtins/src/classloader.rs`'s
`builtin_loader_reachable` (excluding the platform loader from the
"safe to consult the flat global store" set, the same fix this doc's
"second investigation session" update above had already identified as
necessary) — `SpringBootContextLoaderAotTests` now reaches a **different,
earlier** failure instead, during the first phase
(`loadContextForAotProcessing`, before `applyInitializers` is ever reached):

```
java.lang.ExceptionInInitializerError
	at org.springframework.beans.factory.groovy.GroovyBeanDefinitionReader.<init>(GroovyBeanDefinitionReader.java:152)
Caused by: java.lang.NullPointerException: Cannot invoke "org.codehaus.groovy.reflection.ClassInfo.getCachedClass()" because the return value of "org.codehaus.groovy.reflection.ClassInfo.getClassInfo(java.lang.Class)" is null
	at org.codehaus.groovy.reflection.ReflectionCache.getCachedClass(ReflectionCache.java:31)
	at org.codehaus.groovy.reflection.CachedClass$6.initValue(CachedClass.java:179)
```

Confirmed deterministic (reproduces identically across repeated runs) and
**not caused by any fix in this doc** — it is a completely different bug
family (Groovy's own internal `ClassInfo`/`ReflectionCache` global registry
returning no entry for some `Class`, during `MetaClassImpl.setUpProperties`'s
`inheritStaticInterfaceFields`/`addConsts` initialization), unrelated to
loader-identity dispatch. It was previously unreachable simply because the
test never got this far before (the residual chain in this doc, plus the
concurrent `builtin_loader_reachable` fix, together let it progress past
everything documented above). No existing `docs/known-issues` entry covers
this exact `ClassInfo.getClassInfo` NPE (a *different* Groovy MetaClass gap
than `docs/known-issues/springboot/thymeleaf-groovy-layoutdialect-metaclass-introspection-hang.md`,
which is an unbounded-loop hang in `TypeResolver`/`Introspector`, not an NPE
in `ClassInfo`). Not investigated further this session — flagged here as a
new, separate discovery for follow-up; Residual 4 above may or may not still
exist further down the chain once this one is fixed.

### Regression check (third investigation session, 2026-07-20)

All fixes above were verified against the full `core/spring-boot-test`
module (80 test classes) on both CratonVM (with the fixes) and a real
HotSpot baseline for the same class list: **77/80 PASS on both**, identical
2 pre-existing `EMPTY` classes (abstract base classes with no runnable
tests), no regressions. The 2 additional CratonVM-only failures
(`ImportsContextCustomizerFactoryTests`,
`DuplicateJsonObjectContextCustomizerFactoryTests`) were confirmed
**pre-existing** — they reproduce identically (same failure signature) against
the unmodified dev-tip binary, unrelated to any of the fixes in this
session. `SpringBootContextLoaderAotTests` is the sole remaining failure;
as of this branch merging forward with the latest `origin/dev` (see Residual
5 above), it is now blocked earlier, on Residual 5, rather than Residual 4
or the originally-documented NPE — both of which this session's 3 fixes
still resolve correctly.

## Cluster D — missing `"random"` PropertySource — FIXED (upstream, before this session)

`SpringBootContextLoaderTests.propertySourceOrdering()` (1 of 26 tests in the class).

**Verified 2026-07-20: all 26/26 tests PASS**, including `propertySourceOrdering` (`"random"` now correctly present in the property-source list). Not re-root-caused. No action needed.

## Cluster E — `DuplicateJsonObjectContextCustomizerFactoryTests` HANG — REFUTED (does not reproduce)

The originally-hypothesized "regression-in-place-of-fix" (SSLSocketFactory fix causing a real-but-blocked network hang instead of the original crash) does **not** reproduce. **Verified 2026-07-20: PASSES in 1.3-2.5s**, no hang. The `org.json:json:20140107` coordinate used by `@ClassPathOverrides` on this test is already present in `~/.m2/repository` (with a matching `_remote.repositories` marker for `central`), and Aether resolves it from the local cache without needing network I/O in this environment. No action needed; the earlier hang was likely either already fixed alongside another SSL/TLS fix landed between 2026-07-17 and now, or was itself a transient/environmental artifact of that specific rerun.

## Affected classes (updated status)

- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerTests` — **FIXED** (Cluster A)
- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerWithLegacySwitchTests` — **FIXED** (Cluster A)
- `core/spring-boot-test` | `SpringBootTestCustomConfigNameTests` — **FIXED** (Cluster B)
- `core/spring-boot-test` | `SpringBootContextLoaderAotTests` — **OPEN residual** (Cluster C; originally-documented NPE + 2 more residuals FIXED this session; blocked on a 5th, unrelated-bug-family residual — see Cluster C "Residual 5" — with a 4th, distinct, not-yet-root-caused loader-identity residual still waiting behind it, see "Residual 4")
- `core/spring-boot-test` | `SpringBootContextLoaderTests` — **FIXED**, 26/26 (Cluster D)
- `core/spring-boot-test` | `DuplicateJsonObjectContextCustomizerFactoryTests` — **FIXED** / does not reproduce (Cluster E)
