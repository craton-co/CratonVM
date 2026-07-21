# core/spring-boot-test — config-data loading gaps, duplicate classpath scan results, missing PropertySource

**Status: PARTIALLY FIXED (2026-07-21) — Clusters A, B, D, E confirmed fixed upstream (verified against dev tip, HotSpot-matching PASS); Cluster C's originally-documented NPE, Residuals 1-3, and now Residual 5 (Groovy `ClassInfo`/`ClassValue`) are all FIXED and root-caused. Residual 4 (`AotApplicationContextInitializer` loader identity) is no longer reproducible — fixing Residual 5 let the test progress far enough that Residual 4's exact trigger no longer occurs (not independently re-confirmed fixed in isolation; may resurface if Residual 5's fix is ever reverted). `SpringBootContextLoaderAotTests` STILL does not fully pass: under `--nojit` it now reaches a sixth, deeper residual (Spring `ResolvableType`/`TypeVariable` resolution during AOT-runtime replay — "Residual 6", NOT fixed); under JIT it still reproduces Residual 5's ORIGINAL symptom despite the underlying native fix being verified correct and complete — a separate, narrower JIT-only interpreter dispatch-cache bug (deeply traced, one general instance of it fixed as a side effect, but not fully resolved for this exact call site — see Residual 5's writeup below).**

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

### Residual 4 (NO LONGER REPRODUCIBLE as of 2026-07-21 — see Residual 5's fix below)

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

### Residual 5 (FIXED for `--nojit`; JIT has a separate, narrower, NOT fully resolved dispatch-cache issue reproducing the same symptom) — Groovy `ClassInfo.getClassInfo(Class)` returned `null`, unrelated to loader identity

Before merging this branch forward, Residual 4 above (traced against dev
`fba145c60`, this branch's original base) was the observed failure. After
merging the latest `origin/dev` — which independently landed an unrelated,
concurrent fix to `native-builtins/src/classloader.rs`'s
`builtin_loader_reachable` (excluding the platform loader from the
"safe to consult the flat global store" set, the same fix this doc's
"second investigation session" update above had already identified as
necessary) — `SpringBootContextLoaderAotTests` reached a **different,
earlier** failure instead, during the first phase
(`loadContextForAotProcessing`, before `applyInitializers` is ever reached):

```
java.lang.ExceptionInInitializerError
	at org.springframework.beans.factory.groovy.GroovyBeanDefinitionReader.<init>(GroovyBeanDefinitionReader.java:152)
Caused by: java.lang.NullPointerException: Cannot invoke "org.codehaus.groovy.reflection.ClassInfo.getCachedClass()" because the return value of "org.codehaus.groovy.reflection.ClassInfo.getClassInfo(java.lang.Class)" is null
	at org.codehaus.groovy.reflection.ReflectionCache.getCachedClass(ReflectionCache.java:31)
	at org.codehaus.groovy.reflection.CachedClass$6.initValue(CachedClass.java:179)
```

#### Root cause (found + fixed 2026-07-21, worktree `fix/groovy-classinfo-loaderid-residuals-20260720`)

Genuinely a distinct bug family, unrelated to loader identity, confirmed via
`javap` against the real `groovy-5.0.6.jar` (`~/.gradle/caches/modules-2/...`,
the actual version this module's classpath resolves — NOT the 4.0.29 bundled
inside Gradle's own distribution, which has a materially different
`ClassInfo`/`GroovyClassValue` implementation and is a decoy if you go
looking there first): Apache Groovy 5.x's `ClassInfo.getClassInfo(Class)` is
`globalClassValue.get(cls)`, where `globalClassValue` is a
`GroovyClassValueJava7 extends java.lang.ClassValue<ClassInfo>`
(unconditional in 5.x — no `groovy.use.classvalue` opt-out like 4.x had).
Real `java.lang.ClassValue` (javap'd from the actual JDK 25 module image) is
100% real, unmodified bytecode with no native methods of its own — `get()`
depends on CASing a hidden `Class.classValueMap` field via
`jdk.internal.misc.Unsafe`. CratonVM's `java.lang.ClassValue.get`/`remove`
native overrides (`native-builtins/src/phases_late.rs`, inside
`register_p67_misc`) were a stub that unconditionally returned `null`
without ever calling the subclass's `computeValue` — so
`ClassInfo.getClassInfo(cls)` returned `null` for every class, NPEing the
first time any real Groovy `MetaClass` initializes. Confirmed via
`--dump-native-registry` that this was, separately, ALSO simply dead code in
the default (non-`synthetic-jdk`-feature) `cratonvm-cli` build that every
Spring Boot suite run actually uses — `register_p67_misc` is only reachable
via `register_synthetic_overrides`, which `vm/src/vm/vm_init.rs`'s real-JDK
bootstrap branch never calls (`vm/src/native/builtins.rs` even documents
this as an intentional no-op shim for non-synthetic-jdk builds). Both real
JDK bytecode's own Unsafe/`classValueMap` dependency AND the accidental
dead-code placement needed fixing.

**Fix**: new module `native-builtins/src/classvalue_cache.rs` implements
real `ClassValue` semantics — lazily invokes `computeValue` via
`ctx.invoke_virtual` on a cache miss (so `GroovyClassValueJava7`'s override,
or any other `ClassValue` subclass's, runs), caches the result per
`(ClassValue instance, Class)` pair (`null` is itself a valid cached value),
and `remove()` evicts. Cache keys use the same GC-stable identity pattern
established elsewhere in this codebase
(`properties_sidetable.rs::key_for` / `native-collections`'s
`widened_obj_key` / `lib.rs`'s `gc_stable_lock_key` — a raw heap pointer is
not stable under CratonVM's moving GC); cached non-null values are held via
`ctx.add_global_root`/`resolve_global_root` (the same JNI-global-ref-backed,
GC-remapped mechanism used elsewhere for cross-call object retention).
Registered as `NativeKind::Bridge` (a correct implementation of a mechanism
CratonVM cannot run as pure bytecode), not `SyntheticStub`. The
registration is now called explicitly from BOTH
`register_p67_misc` (unchanged code path, kept for the synthetic-jdk build)
AND, newly, `vm/src/vm/vm_init.rs`'s real-JDK-mode branch directly (same
"keep the default CLI's real-JDK registration in sync with the
synthetic-feature build above" pattern already used there for
`register_p60_process_handle`) — see `register_classvalue_natives`'s doc
comment in `classvalue_cache.rs` for the full writeup of the dead-code trap.

**Verification**: under `--nojit`, `SpringBootContextLoaderAotTests`'s
`ClassInfo.getClassInfo` NPE is completely gone — Phase 1
(`loadContextForAotProcessing`) now completes with NO errors at all
(Residual 4 above no longer reproduces as a side effect — not
independently re-confirmed in isolation). The test still fails, now in
**Phase 2** (`loadContextForAotRuntime`) on a new, deeper issue — see
Residual 6 below. Full `core/spring-boot-test` module regression (80
classes, both `--nojit` and default JIT): **75 PASS / 2 EMPTY / 3 FAIL,
identical failure set in both modes**
(`ImportsContextCustomizerFactoryTests` — pre-existing `AssertionFailedError`,
unrelated to any fix here; `DuplicateJsonObjectContextCustomizerFactoryTests`
— a test-discovery-time classpath resolution error for its
`@ClassPathOverrides` fork, environment-dependent, unrelated to any fix
here; `SpringBootContextLoaderAotTests` — see below). No regressions vs.
the previously-documented 77/80 baseline.

#### JIT-only residual (deeply traced, NOT fully resolved): the exact `ClassInfo.getClassInfo` NPE above still reproduces under default (JIT-on) mode

Despite the native fix above being verified correct and complete
(`CRATONVM_TRACE_CLASSVALUE=1` — a permanent, env-gated trace now built into
`classvalue_get`/`classvalue_remove` — shows it being called ~2500 times
per run, NEVER once returning `null`), running the exact same test under
JIT (the suite's default) still reproduces the ORIGINAL
`ClassInfo.getClassInfo` NPE verbatim. This means the failing call is not
reaching the native at all under JIT, for reasons distinct from (deeper
than) the native's own correctness.

One real, general interpreter bug was found and fixed along the way:
`vm/src/runtime/interpreter.rs`'s `execute_invokevirtual_cached` instance-
method JIT tier-up (`bug-03 layer B`) JIT-compiles a hot instance method's
real bytecode once its per-call-site counter crosses a threshold, and its
own comment claims natively-overridden methods are excluded by that point
— true only for FORCED natives
(`force_native_over_real_jdk_bytecode`'s allow-list, checked by
`intercept_force_registered_native_cached` immediately above); an ordinary,
non-forced registered native (the common/default case — natives shadow
bytecode by default elsewhere in the interpreter) was invisible to it, so
tier-up would compile-and-permanently-cache the method's REAL bytecode,
silently bypassing the native forever afterward for that receiver class
(compiled code doesn't re-run the interpreter's native-vs-bytecode
decision per call). This is at least the 4th independent occurrence of the
identical bug shape across different caching subsystems in this
interpreter (see the "Cached bytecode bypasses native dispatch, which must
retain precedence" comments already present at the lambda-impl bytecode
cache, ~`interpreter.rs:21478`, and the "third occurrence of the same gap"
comment at the JIT-callee compiler, ~`interpreter.rs:33006`) — worth a
systemic audit of every `CachedBytecodeMethod`/`jit_cache` construction
site for the same missing guard. Fixed generically (reusing the
already-memoized `cached.native_callback_cache`, so zero extra per-call
cost) by adding a `has_registered_native` guard before the tier-up
decision, so this exact bug shape can no longer bite ANY natively-overridden
instance method that becomes tier-up-eligible, not just `ClassValue.get`.
Verified via the same full-module regression above (identical PASS/FAIL
counts JIT vs. `--nojit`) — this fix causes no regressions.

**But it did not, by itself, fix `ClassValue.get()`'s JIT symptom.**
Isolating this further requires execution-level tracing beyond what static
reading of the dispatch code can settle — deep investigation this session
narrowed it to `execute_invokevirtual_cached`'s per-call-site
`thread.invoke_cache` (keyed by `(caller_class_id, cp_index, is_special)`),
which resolves to one of `CachedInvokeTarget::VirtualNative` (native,
unconditional) or `::VirtualBytecode` (interpreted/compiled real bytecode,
where BOTH the force-gate and the tier-up guard above only apply) — but
NOT which of the two `ClassInfo.getClassInfo`'s one `invokeinterface
GroovyClassValue.get` call site actually resolves to under JIT, nor why
that might differ from `--nojit`, nor whether it's a first-resolution bug
or (again) a loader-duplication artifact (this whole doc's recurring
theme — `@CompileWithForkedClassLoaderClassLoader` is exactly the kind of
scenario that has repeatedly produced two independent copies of a class
elsewhere in this cluster). **Next steps for whoever picks this up**: trace
`thread.invoke_cache.get`/`.insert` (or add a temporary
`CRATONVM_DBG_*`-gated print at the `VirtualNative`/`VirtualBytecode`
match arms in `execute_invokevirtual_cached`, ~`interpreter.rs:36391`)
filtered to `class_name == "java/lang/ClassValue"`, comparing a JIT run
against a `--nojit` run of the exact same test to see which variant gets
cached and from where.

### Residual 6 (NEW, found 2026-07-21 under `--nojit` after Residual 5's fix, deeply traced, NOT fixed): Spring's `ResolvableType` can't resolve a lambda's inherited-interface `TypeVariable` during AOT-runtime replay

With Residual 5 fixed, `--nojit` progresses past Phase 1 entirely (no
errors) and now fails in **Phase 2** (`loadContextForAotRuntime` — running
the AOT-GENERATED, freshly `TestCompiler`-recompiled bytecode, a different
code path than Phase 1):

```
java.lang.IllegalStateException: No generic type found for initializr of type class org.springframework.boot.test.context.SpringBootContextLoader$ContextLoaderHook$1$$Lambda/...
	at org.springframework.util.Assert.state(Assert.java:102)
	at org.springframework.boot.SpringApplication.applyInitializers(SpringApplication.java:620)
```
("initializr" is literally in Spring's own message text, not a typo here.)

Real source (`apps/spring-boot/core/spring-boot-test/src/main/java/org/springframework/boot/test/context/SpringBootContextLoader.java:569`):
```java
application.addInitializers(
    (AotApplicationContextInitializer<?>) ContextLoaderHook.this.initializer::initialize);
```
A bound method-reference lambda (`initializer::initialize`, `initializer`
a captured instance field) cast to `AotApplicationContextInitializer<?>` —
the same general family as Residuals 2-3 above (lambda functional-interface
generic-type reconstruction), but this specific call site still fails
despite residuals 2-3's existing, general-purpose fixes
(`lambda_proxy_satisfies`'s loader-aware fallback,
`lambda_functional_interface_generic_type` in
`native-builtins/src/generics.rs`).

Traced with the existing `CRATONVM_DBG_LAMBDA_GENERIC=1` flag (already
covers `AotApplicationContextInitializer` as a substring match, no code
changes needed to observe it):

1. `lambda_functional_interface_generic_type`'s own bounded walk succeeds —
   correctly determines the lambda's SAM (`initialize`) is declared on
   `ApplicationContextInitializer` (one level up from
   `AotApplicationContextInitializer`, which merely forwards `C`), and
   produces `AotApplicationContextInitializer<ConfigurableApplicationContext>`
   as the lambda's own `getGenericInterfaces()` result. Not the bug.
2. Real Spring bytecode (`ResolvableType.as(ApplicationContextInitializer.class)`)
   then calls `AotApplicationContextInitializer.class.getGenericInterfaces()`
   (the real class, not the lambda) — correctly returns
   `[ApplicationContextInitializer<TypeVar("C")>]`, a BARE, unsubstituted
   type variable. Confirmed this is correct JVM semantics (a `Class`
   object's own generic supertype legitimately contains its own unbound
   formal type parameters per JLS/JVMS) — not itself a bug.
3. Spring's `ResolvableType` machinery needs to resolve this bare
   `TypeVariable` "C" back to a concrete type by tracing it through its own
   accumulated substitution context (matching it against the lambda-level
   parameterization from step 1). The trace goes silent immediately after
   `getGenericInterfaces ENTRY this_name=org/springframework/context/ApplicationContextInitializer`
   (`ApplicationContextInitializer` itself, a plain interface with no
   super-interfaces — legitimately returns `[]`, also not a bug). No
   exception, no further `native-builtins` trace output at all — the
   actual give-up happens entirely inside real Spring/JDK bytecode, most
   likely because the `TypeVariable` Java object CratonVM hands back for
   "C" (`type_sig_to_java`'s `TypeVar` branch /
   `native-builtins/src/lang_class.rs`, near the "resolved via the
   generic-decl scope" doc comment on `typesig_to_real_type`) isn't a
   fully spec-faithful `sun.reflect.generics.reflectiveObjects.TypeVariableImpl`
   — e.g. its `getGenericDeclaration()`/identity may not let
   `ResolvableType`'s real bytecode trace it back to where it was
   originally bound (`ResolvableType` uses a
   `Map<TypeVariable, ResolvableType>` internally in places, so
   `equals()`/`hashCode()` fidelity matters too, not just the name).

**Not fixed.** Next steps for whoever picks this up: this needs a different
investigation technique than `CRATONVM_DBG_LAMBDA_GENERIC` (which only
covers `native-builtins`, not what real Spring bytecode does with the
`TypeVariable` object afterward) — either a stack-dump/breakpoint-style
trace of `ResolvableType`'s own execution, or directly comparing what a
real `TypeVariableImpl` for this exact "C" declaration looks like on
HotSpot vs. CratonVM's synthetic stand-in. Given residuals 2-3 already
built real machinery for lambda-functional-interface generics, the fix
likely lives in strengthening whatever produces the `TypeVariable` object
for a bare `TypeSig::TypeVar` in `typesig_to_real_type`/`type_sig_to_java`
to be a real, spec-faithful reflective object, or in reconstructing it
pre-substituted (the way `lambda_functional_interface_generic_type`
already does one level up) instead of leaving a bare unresolved `TypeVar`
for Spring to chase down itself.

### Regression check (2026-07-21, this session)

Full `core/spring-boot-test` module (80 test classes), both `--nojit` and
default JIT, against `apps/spring-boot` at `C:\craton\CratonVM\apps\spring-boot`
(shared checkout): **75 PASS / 2 EMPTY / 3 FAIL in both modes, identical
failure set** — see the Residual 5 section above for per-class detail.
Matches the previously-documented 77/80 (PASS+EMPTY) baseline exactly; no
regressions from any fix in this session.

## Cluster D — missing `"random"` PropertySource — FIXED (upstream, before this session)

`SpringBootContextLoaderTests.propertySourceOrdering()` (1 of 26 tests in the class).

**Verified 2026-07-20: all 26/26 tests PASS**, including `propertySourceOrdering` (`"random"` now correctly present in the property-source list). Not re-root-caused. No action needed.

## Cluster E — `DuplicateJsonObjectContextCustomizerFactoryTests` HANG — REFUTED (does not reproduce)

The originally-hypothesized "regression-in-place-of-fix" (SSLSocketFactory fix causing a real-but-blocked network hang instead of the original crash) does **not** reproduce. **Verified 2026-07-20: PASSES in 1.3-2.5s**, no hang. The `org.json:json:20140107` coordinate used by `@ClassPathOverrides` on this test is already present in `~/.m2/repository` (with a matching `_remote.repositories` marker for `central`), and Aether resolves it from the local cache without needing network I/O in this environment. No action needed; the earlier hang was likely either already fixed alongside another SSL/TLS fix landed between 2026-07-17 and now, or was itself a transient/environmental artifact of that specific rerun.

## Affected classes (updated status)

- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerTests` — **FIXED** (Cluster A)
- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerWithLegacySwitchTests` — **FIXED** (Cluster A)
- `core/spring-boot-test` | `SpringBootTestCustomConfigNameTests` — **FIXED** (Cluster B)
- `core/spring-boot-test` | `SpringBootContextLoaderAotTests` — **OPEN residual** (Cluster C; originally-documented NPE + residuals 1-3 FIXED (2026-07-20 session); Residual 5 (Groovy `ClassInfo`/`ClassValue`) FIXED for `--nojit` (2026-07-21 session) — Residual 4 no longer reproduces as a side effect; under `--nojit` now blocked on a NEW, deeper Residual 6 (Spring `ResolvableType`/`TypeVariable` resolution, NOT fixed); under JIT still reproduces Residual 5's original symptom via a separate, narrower, NOT fully resolved interpreter dispatch-cache bug (one general instance of that bug class fixed as a side effect) — see Cluster C "Residual 5"/"Residual 6")
- `core/spring-boot-test` | `SpringBootContextLoaderTests` — **FIXED**, 26/26 (Cluster D)
- `core/spring-boot-test` | `DuplicateJsonObjectContextCustomizerFactoryTests` — **FLAKY/environment-dependent** (Cluster E fix — the original HANG — still holds; a DIFFERENT, test-discovery-time classpath resolution error for its `@ClassPathOverrides` fork reproduced in this session's regression run, unrelated to any fix in this doc — see Residual 5's regression-check paragraph)
