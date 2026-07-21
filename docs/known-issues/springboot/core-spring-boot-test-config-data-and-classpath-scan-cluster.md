# core/spring-boot-test — config-data loading gaps, duplicate classpath scan results, missing PropertySource

**Status: FIXED under interpreter execution (2026-07-20/21, fourth+fifth investigation sessions) — Clusters A, B, D, E confirmed fixed upstream; Cluster C's originally-documented NPE plus Residuals 1–5 are now ALL fixed, unconditionally, including under heavy parallel-sweep host contention. `SpringBootContextLoaderAotTests` PASSES end-to-end under `-Jit off` — verified 10+ times across isolated and 3-way/6-way concurrent-process stress runs with no failures. (An earlier version of this doc reported an apparent "load-dependent" recurrence of Residual 4 under contention; that was a methodology error in the investigating session — its parallel-sweep test runs silently used a stale pre-fix binary due to an environment-variable-propagation bug in how the test runner was invoked, not a real second race. See Residual 4's note below for detail and the general lesson.) One caveat remains open and IS real: under `-Jit on` (the suite's default), the class still fails with Residual 5's original NPE signature — a JIT-specific dispatch gap, confirmed with a verified-fresh binary, see "Residual 6".**

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

### Residual 4 (FIXED, fourth investigation session): `AotApplicationContextInitializer` itself ends up Application-loader-owned

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

**Root cause (fourth investigation session, 2026-07-20):** hypothesis 1 from
the original writeup ("another static-method-reference lambda") was correct,
just one MethodHandleKind off. `CRATONVM_DBG_LAMBDA_GENERIC=1` tracing showed
the loader-blind resolution actually happens in TWO places, both reached via
the SAM-generic-type machinery Residual 3's fix added:

1. `native-builtins/src/lang_class.rs`'s lambda branch of
   `native_class_get_generic_interfaces` calls
   `crate::generics::typesig_to_real_type(ctx, &sig)` to turn the lambda's own
   functional-interface signature (e.g.
   `AotApplicationContextInitializer<ConfigurableApplicationContext>`) into a
   real `Type`, but never establishes a `GenericDeclScope` first — unlike the
   "real class" branch a few lines up, which does. `typesig_to_real_type`
   resolves the interface's own name via `class_id_in_generic_scope`, whose
   `near` anchor comes from that scope; with no scope set, `near=None` and it
   falls straight to the loader-blind global lookup — observed
   (`decl_present=false near=None ... global=Some(ClassId(4623))`) picking up
   whichever copy the flat store already held (the Application loader's).
2. `native-builtins/src/generics.rs`'s `lambda_functional_interface_generic_type`
   walks the interface's `extends` chain (needed here because
   `AotApplicationContextInitializer<C> extends ApplicationContextInitializer<C>`
   merely forwards `C`, never redeclaring the SAM) via a bare
   `ctx.class_id_by_name(next_name)` — also loader-blind.

**Fix:**
- `lang_class.rs`: before calling `typesig_to_real_type` in the lambda
  branch, resolve the lambda's host class (`ctx.lambda_proxy_host(class_id)`,
  already correctly fork-loader-resolved by the time the lambda exists — it's
  not a first-touch) and wrap the call in
  `GenericDeclScope::new(Value::Object(Some(host_mirror)))`, mirroring the
  "real class" branch's own scoping.
- `generics.rs`: the `extends`-chain walk now tries
  `ctx.class_id_by_name_near(next_name, current_id)` first, falling back to
  the loader-blind `class_id_by_name` only on a miss — same pattern as every
  other loader-aware resolution in this cluster.
- `interpreter.rs`: also mirrored the already-fixed `InvokeStatic` lambda-impl
  loader gap (`lambda_impl_dispatch_override_driven`, Residual-1's own fix)
  onto the sibling `MethodHandleKind::NewInvokeSpecial` case in
  `try_lambda_dispatch`, which had the exact same passive-cache-then-loader-blind-fallback
  shape for constructor-reference lambdas
  (`SomeType::new`, the mechanism behind AOT-generated
  `AotApplicationContextInitializer::new`-style factories). This didn't turn
  out to be the actual fix for this specific failure (the two `generics.rs`/
  `lang_class.rs` fixes above were), but it closes a real, structurally
  identical gap for the sibling dispatch kind and is kept — dev independently
  landed the same fix concurrently, confirming it was worth having.
- `lang_class.rs` (fifth investigation session, 2026-07-21): a third
  loader-blind lookup, `ctx.class_id_by_name(&iface_name)` resolving a
  lambda's SAM interface's own `ClassId` (used both by
  `native_class_get_interfaces` — `Class.getInterfaces()` — and as
  `lambda_functional_interface_generic_type`'s starting `iface_id` in
  `native_class_get_generic_interfaces`), was hardened the same way via a
  new shared `lambda_functional_interface_id_loader_aware` helper scoped to
  `ctx.lambda_proxy_host`. Not proven to be load-bearing for this specific
  failure (see the correction below), but it's the same real gap pattern and
  a legitimate hardening regardless.

**Verified, unconditionally — the "load-dependent" finding in an earlier
version of this doc was a methodology error, corrected 2026-07-21.** An
earlier investigating session ran `SpringBootContextLoaderAotTests` inside a
large parallel sweep (`-Subtrees core -Parallel 6`, ~570 classes) via
`powershell.exe -Command "$env:CV_BIN=...; & run-spring-boot-suite.ps1 ..."`
launched from a Bash background job, saw the *original* pre-fix
`IllegalStateException` recur, and concluded Residual 4 had a second,
load-dependent race. It did not: `$env:CV_BIN` set **inside** a
`powershell.exe -Command` string invoked **from Bash** does not reliably
reach the child `run-spring-boot-suite.ps1` process — the runner silently
fell through to its `target\release\cratonvm-spring-boot-suite.exe`
auto-copy fallback, which is only refreshed from `cratonvm.exe` when it
doesn't already exist. A copy of that file created hours earlier (build
`build4`/`build5`, before the Residual 4 fixes even fully landed) was still
sitting in `target\release\`, and every subsequent stress run in that
session silently ran against it — confirmed post hoc by checking each run's
own `craton exe=` log line, which named the stale file every time. Passing
`-Exe <absolute path>` explicitly (highest-priority resolution, ahead of
`$env:CV_BIN`) instead of relying on the environment variable makes this
class of error impossible; re-running the exact same stress shape with `-Exe`
set explicitly — 3-way, then 6-way concurrent `-Parallel 1`-style isolated
processes racing each other, 9 runs total — **PASSED every single time**.
**General lesson for future sessions**: launching `run-spring-boot-suite.ps1`
via `powershell.exe -Command "..."` from a *backgrounded Bash* command is
unreliable for environment-variable propagation; always pass `-Exe` with an
absolute path instead of `$env:CV_BIN` when doing this, and always check the
run's own `craton exe=` log line before trusting a "surprising" result from
a test invoked this way.

### Residual 5 (FIXED, fourth investigation session): Groovy `ClassInfo.getClassInfo(Class)` returns `null`, unrelated to loader identity

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
in `ClassInfo`).

**Root cause (fourth investigation session, 2026-07-20):**
`org.codehaus.groovy.reflection.v7.GroovyClassValueJava7 extends
java.lang.ClassValue<T>`, and `ClassInfo.getClassInfo(Class)` is
`globalClassValue.get(cls)` — real `java.lang.ClassValue.get()`.
`native-builtins/src/phases_late.rs` registered `java/lang/ClassValue.get`
as a **native stub that always returned `null`** instead of dispatching to
the receiver's `computeValue(Class)` override (documented separately in
`docs/internal/CRATONVM_BUGS/BUG-W-classvalue-get-null-stub.md`, whose own
"routing to `computeValue` doesn't help" conclusion was scoped to a
*different* target — `MethodHandleImpl$ArrayAccessor$1.computeValue`, which
fails for an unrelated reason, unimplemented MethodHandle array-access
intrinsics). For Groovy's `ClassInfo$1.computeValue` — plain
`new ClassInfo(type); GlobalClassSet.add(...); return classInfo` — dispatching
to `computeValue` is exactly correct and sufficient.

**Fix:** `get()` now dispatches to `computeValue(Class)` via
`ctx.invoke_virtual` **with memoization** (the earlier attempt dropped it;
real `ClassValue.get()` calls `computeValue` **at most once** per
`(instance, Class)` and caches the result forever after — Groovy's
`MetaClassRegistryImpl`/`ExpandoMetaClass` mutate a `ClassInfo` in place and
depend on that same-instance identity on every later `getClassInfo(cls)`
call). The cache is keyed by
`(identity_hash_code(this), identity_hash_code(cls))` — both stable across a
moving GC, so unlike most process-global object caches in this crate, the
*keys* need no pointer-remap bookkeeping; only the cached *values* are
reported as GC roots and remapped (`gc_scan_classvalue_cache_roots` /
`gc_update_classvalue_cache_refs` in `phases_late.rs`, wired into
`roots.rs`/`gc.rs`, cleared on VM (re)creation via `reset_classvalue_cache`
in `vm_init.rs`).

**Verified:** with the fix, `SpringBootContextLoaderAotTests` no longer hits
this NPE under `-Jit off` and instead progresses straight to (now also
fixed) Residual 4.

**Residual 6 (OPEN, newly discovered): the same fix doesn't take effect under `-Jit on`.**
Under the suite's default `-Jit on`, `SpringBootContextLoaderAotTests` still
fails with this exact NPE — deterministic across repeated runs, both before
and after the Residual 4 fixes above (which are semantically independent and
correctly took effect under both JIT settings once reached). Two dispatch-gate
allow-lists that are known to need a matching entry for a class that has both
real JDK bytecode and a registered native override —
`force_native_over_real_jdk_bytecode` (`interpreter.rs`) and the
`check_override` gate inside `invoke_on_class_shared_inner`
(`vm_exec.rs`) — were both given a `java/lang/ClassValue.get` entry as a
plausible fix; neither changed the outcome, and `CRATONVM_JIT_OSR=0` (the
escape hatch for the unrelated, already-tracked
`reference_jit_osr_loop_duplicate_execution` critical bug) didn't either.
**Not yet root-caused.** Whatever dispatch path a JIT-compiled *caller* of
`ClassValue.get()` uses to decide native-vs-bytecode, it is provably
different from the interpreter's `try_stackless_invoke`/`invoke_or_native`
ancestor walk (confirmed correct and sufficient by the `-Jit off` PASS) and
doesn't consult either of the two allow-lists above. Next step: find the
JIT-specific call-target resolution for a native method inherited by a
receiver from a superclass reached via `invokeinterface` (`vm/src/jit/`), or
instrument it directly rather than continuing to guess at allow-list gaps.

### Regression check (fourth investigation session, 2026-07-20; corrected fifth session, 2026-07-21)

Re-ran the full `core/spring-boot-test` module (81 test classes; the extra
one relative to the third session's 80 is enumeration noise, not a new
class) on CratonVM with all of this session's fixes, `-Jit on`, against the
same-day HotSpot baseline used previously: **exact match except the same 2
pre-existing CratonVM-only failures already documented above
(`ImportsContextCustomizerFactoryTests`,
`DuplicateJsonObjectContextCustomizerFactoryTests` — both reproduce
identically, unrelated to this session's changes) plus
`SpringBootContextLoaderAotTests` itself (blocked on Residual 6 under
`-Jit on` only)** — no new regressions. The broader `core` Gradle subtree
(573 classes across `core/spring-boot`, `-autoconfigure`,
`-docker-compose`, `-test`, `-test-autoconfigure`, `-testcontainers`) was
also swept incidentally via `-Subtrees core`; none of its ~50 failures
outside `core/spring-boot-test` (logging/config/YAML/SSL — no overlap with
`ClassValue` or lambda-generics call paths) were compared against a
same-session baseline and are out of this doc's scope — flagged here only so
a future session doesn't mistake them for something this session touched.

A `-Jit off` full-module run in the fourth session appeared to show
`SpringBootContextLoaderAotTests` **FAIL** with the *original*
pre-Residual-4-fix `IllegalStateException`, prompting an incorrect
"load-dependent second race" conclusion. **Corrected in the fifth session
(2026-07-21)**: that run's own log line
(`craton exe=...\cratonvm-spring-boot-suite.exe`) shows it silently used a
stale binary from hours earlier, not the session's actual fix — see the
"Verified, unconditionally" note under Residual 4 above for the full
explanation. Re-running the identical 573-class `-Subtrees core -Jit off
-Parallel 6` sweep with the binary passed explicitly via `-Exe` (confirmed
fresh via its own `craton exe=` log line) shows `SpringBootContextLoaderAotTests`
**PASS**, matching the HotSpot baseline exactly — it is no longer one of the
divergences. The only 3 divergences from baseline in this corrected run are
the same 2 pre-existing failures plus one new one, unrelated to this
session's changes: `UriBuilderFactoryWebConnectionHtmlUnitDriverTests`
**HANG** (300s timeout; an HtmlUnit/HTTP-client test, no overlap with
`ClassValue` or lambda-generics — almost certainly the same shared-host
CPU-contention class of artifact as `DuplicateJsonObjectContextCustomizerFactoryTests`'s
flakiness, not investigated further as out of this doc's scope).

## Cluster D — missing `"random"` PropertySource — FIXED (upstream, before this session)

`SpringBootContextLoaderTests.propertySourceOrdering()` (1 of 26 tests in the class).

**Verified 2026-07-20: all 26/26 tests PASS**, including `propertySourceOrdering` (`"random"` now correctly present in the property-source list). Not re-root-caused. No action needed.

## Cluster E — `DuplicateJsonObjectContextCustomizerFactoryTests` HANG — REFUTED (does not reproduce), but FLAKY under parallel load

The originally-hypothesized "regression-in-place-of-fix" (SSLSocketFactory fix causing a real-but-blocked network hang instead of the original crash) does **not** reproduce. **Verified 2026-07-20: PASSES in 1.3-2.5s**, no hang. The `org.json:json:20140107` coordinate used by `@ClassPathOverrides` on this test is already present in `~/.m2/repository` (with a matching `_remote.repositories` marker for `central`), and Aether resolves it from the local cache without needing network I/O in this environment. No action needed; the earlier hang was likely either already fixed alongside another SSL/TLS fix landed between 2026-07-17 and now, or was itself a transient/environmental artifact of that specific rerun.

**Update (fourth investigation session, 2026-07-20):** re-observed as `FAIL` (not hang) during the full-module regression run at `-Parallel 6` — `org.junit.platform.launcher.core.DiscoveryIssueException: ... could not be resolved`, a JUnit-Platform test-discovery-level error, not a CratonVM runtime failure, and not reproduced when this doc's target class was run in isolation. Consistent with `@ClassPathOverrides`' Aether resolution racing with 5 sibling processes over the same local `.m2` cache/lockfile under parallel load — see `reference_shared_host_multitenant_confound` — not a regression from this session's fixes. Re-verify in isolation (`-Parallel 1`) before treating any future occurrence as a real bug.

## Affected classes (updated status)

- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerTests` — **FIXED** (Cluster A)
- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerWithLegacySwitchTests` — **FIXED** (Cluster A)
- `core/spring-boot-test` | `SpringBootTestCustomConfigNameTests` — **FIXED** (Cluster B)
- `core/spring-boot-test` | `SpringBootContextLoaderAotTests` — **FIXED under `-Jit off`, unconditionally** (Cluster C; originally-documented NPE + Residuals 1–5 all FIXED, PASSES end-to-end — verified isolated and under 3-way/6-way concurrent-process contention, 10+ runs, no failures); **OPEN under `-Jit on`** (the suite default) — see "Residual 6", a real, separate, not-yet-root-caused JIT dispatch gap (confirmed with a verified-fresh binary, not a methodology artifact)
- `core/spring-boot-test` | `SpringBootContextLoaderTests` — **FIXED**, 26/26 (Cluster D)
- `core/spring-boot-test` | `DuplicateJsonObjectContextCustomizerFactoryTests` — **FIXED** / does not reproduce in isolation; flaky under parallel-load regression runs (Cluster E, see update above) — unrelated to this session's changes
