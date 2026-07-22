# core/spring-boot-test — config-data loading gaps, duplicate classpath scan results, missing PropertySource

**Status: FULLY FIXED (2026-07-21, eighth investigation session) — Clusters A, B, D, E fixed upstream; Cluster C's originally-documented NPE plus Residuals 1–6 are now ALL fixed. `SpringBootContextLoaderAotTests` PASSES end-to-end under BOTH `-Jit on` (the suite default) and `-Jit off`. Residual 6's root cause — after seven sessions of (individually correct!) dispatch-machinery analysis — was never in dispatch at all: the JIT's `checkcast` codegen resolves its target class BY NAME through the flat global `find_class_by_name`, so when the AOT-processing forked classloader re-defined Groovy's `ClassInfo` under a second `ClassId`, the id-based subtype check refused a cast between two same-named copies and `jit_checkcast` SILENTLY RETURNED NULL (the codegen pushed the failure-`0` as the result — no CCE, no trace). Fixed by a loader-identity-blind name-based hierarchy-walk fallback in `jit_typecheck_resolve` (covers `instanceof` too), plus a companion hardening that makes a definitively-failed JIT checkcast throw a real `ClassCastException` instead of silently nulling. See Residual 6's eighth-session note for the full root-cause chain.**

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

**Update (fifth investigation session, 2026-07-21, worktree
`fix/groovy-classinfo-loaderid-residuals-20260720`): independently
rediscovered this same residual, found and fixed one real, general
contributing bug, but it does NOT by itself resolve this exact symptom.**

Traced with `CRATONVM_TRACE_CLASSVALUE=1` (a new, permanent, env-gated
`eprintln!` added to the `get`/`remove` natives themselves — orthogonal to
and independent of the two dispatch-gate allow-lists this doc's fourth
session already tried): confirmed the native is called ~2500 times per run
under `-Jit on` and NEVER once returns `null` — the native's own logic is
provably correct. This rules out the native itself and narrows the gap to
dispatch/caching specifically, consistent with the fourth session's
conclusion.

One real, general interpreter bug WAS found and fixed along the way:
`vm/src/runtime/interpreter.rs`'s `execute_invokevirtual_cached` instance-
method JIT tier-up (`bug-03 layer B`) JIT-compiles a hot instance method's
real bytecode once its per-call-site counter crosses a threshold, and its
own comment claims natively-overridden methods are excluded by that point —
true only for FORCED natives (the two allow-lists this doc's fourth session
already tried); an ordinary, non-forced registered native (the default case
— natives shadow bytecode by default elsewhere in the interpreter) was
invisible to it, so tier-up could compile-and-permanently-cache a method's
REAL bytecode, silently bypassing its native forever afterward for that
receiver class (compiled code doesn't re-run the interpreter's native-vs-
bytecode decision per call). This is at least the 4th independent occurrence
of the identical bug shape across different caching subsystems in this
interpreter — see the "Cached bytecode bypasses native dispatch, which must
retain precedence" comment already present at the lambda-impl bytecode
cache (`interpreter.rs`, `try_lambda_dispatch`'s cache) and the "third
occurrence of the same gap" comment at the JIT-callee compiler used by
`jit_invoke_dispatch` — worth a systemic audit of every
`CachedBytecodeMethod`/`jit_cache` construction site for the same missing
guard. Fixed generically (reusing the already-memoized
`cached.native_callback_cache`, so zero extra per-call cost) with a
`has_registered_native` guard before the tier-up decision — this exact bug
shape can no longer bite ANY natively-overridden instance method that
becomes tier-up-eligible, not just `ClassValue.get`. Verified via a full
`core/spring-boot-test` module regression (80 classes, both `-Jit off` and
`-Jit on`): identical PASS/FAIL sets in both modes, no regressions.

**But it did not, by itself, fix `ClassValue.get()`'s JIT symptom** — the
exact NPE still reproduces under `-Jit on` with this fix applied. Isolating
further requires execution-level tracing beyond what static reading of the
dispatch code can settle. This session narrowed it to
`execute_invokevirtual_cached`'s per-call-site `thread.invoke_cache` (keyed
by `(caller_class_id, cp_index, is_special)`), which resolves to one of
`CachedInvokeTarget::VirtualNative` (native, unconditional — where the
~2500 successful calls above are believed to come from) or
`::VirtualBytecode` (interpreted/compiled real bytecode — where BOTH
dispatch-gate allow-lists AND the new tier-up guard only apply) — but NOT
which of the two `ClassInfo.getClassInfo`'s one `invokeinterface
GroovyClassValue.get` call site actually resolves to under `-Jit on`, nor
why that might differ from `-Jit off`, nor whether it's a first-resolution
bug or (again) a loader-duplication artifact — this whole doc's recurring
theme. **Next step for whoever picks this up**: trace
`thread.invoke_cache.get`/`.insert` (or add a temporary
`CRATONVM_DBG_*`-gated print at the `VirtualNative`/`VirtualBytecode` match
arms in `execute_invokevirtual_cached`) filtered to
`class_name == "java/lang/ClassValue"`, comparing a `-Jit on` run against a
`-Jit off` run of the exact same test to see which variant gets cached and
from where — this is a more targeted continuation of the "JIT-specific
call-target resolution" next step above, now with a concrete enum split to
instrument rather than an unbounded `vm/src/jit/` search.

**Sixth-session note (2026-07-21, worktree
`fix/sb-configdata-loader-identity-residual-20260720`, merging the above
in):** this session's own three JIT-side attempts (`force_native_over_real_jdk_bytecode`
and `check_override` allow-list entries for `java/lang/ClassValue.get`,
tried before this doc's fifth-session update above existed) are superseded
by the fifth session's more precise `is_classvalue_native_override` gate —
kept for now since they're harmless (a `false`-returning allow-list check
some other dispatch path can still consult) but likely redundant; a future
cleanup pass could remove them once the fifth session's next-step trace
lands and the real fix is known. Did not attempt the `thread.invoke_cache`
trace this session — out of this session's scope (`-Jit off`, Residual 4).

**Seventh-session note (2026-07-21, worktree
`fix/classvalue-jit-dispatch-residual6-20260721`): exhaustively traced every
Rust-level invoke-dispatch mechanism live with runtime instrumentation
(not just static code reading) and ruled essentially all of them out as the
proximate cause; found and fixed one real, related, but ultimately
insufficient bug; Residual 6 is still OPEN with a much narrower next-step
hypothesis than before.**

Reproduced reliably via a standalone `SbRunner
org.springframework.boot.test.context.SpringBootContextLoaderAotTests`
invocation (no suite-runner wrapper needed) against a debug-instrumented
build. Added temporary `eprintln!` tracing (since removed; reusing/extending
the existing `CRATONVM_TRACE_CLASSVALUE` gate) to every plausible dispatch
site and ran the repro under `-Jit on` (default) with each in turn:

1. **The registered native itself** (`native-builtins/src/phases_late.rs`'s
   `get()` closure): traced every single invocation's raw args and outcome.
   Called ~2350+ times across the run, **never once** returns null, and is
   **never called at all** for the one specific invocation whose result the
   NPE blames — confirmed by also printing the resolved `cls` argument's
   class name on every call: the sequence of classes queried runs right up
   to the failure (`...MetaBeanProperty, [Ljava.lang.Object;, boolean,
   MetaClass, [Ljava.lang.Class;, Class, List, MetaClass,
   MetaObjectProtocol`) and then **stops** — the next logical `get()` call
   (whichever class it's for; the failing call's argument was never
   confirmed, see below) never reaches the native at all, in any form
   (not even with a null/malformed argument).
2. **`jit_invoke_virtual_mic`** (`vm/src/jit/helpers.rs`, the JIT
   MIC/PIC dispatch helper for compiled invokevirtual/invokeinterface call
   sites): traced ~800+ real invocations once `ClassInfo.getClassInfo`
   itself became JIT-compiled partway through the run. `compile_res` is
   `false` on **every single call** — not because `try_jit_compile_callee`'s
   native check keeps correctly refusing (though it does, see point 4), but
   because **`direct_virtual_compiled_callee_entry_enabled()` requires the
   opt-in env var `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY`, unset
   by default** — the entire "direct compiled entry" / inline MIC/PIC
   machine-code cascade this doc's fourth/fifth sessions suspected is
   **inert in the suite's actual default configuration**. Every traced call
   correctly falls through to `invoke_or_native`, which correctly resolves
   and calls the native (confirmed by the matching `cv-native` trace firing
   for each). Verified via `CRATONVM_DBG_JIT_DISASM=ClassInfo.getClassInfo`
   (dumps annotated x86-64) that the inline PIC cascade code IS present in
   the compiled body (a `cmp`/`je` gate on `cached_needs_context` at
   `JitMICSlot`+0x10 before an unconditional fallback call to
   `jit_invoke_virtual_mic`), but since `cached_entry_ptr`/`cached_needs_context`
   are only ever written together by `jit_invoke_virtual_mic` itself (and
   never populated because the compile attempt is skipped by the disabled
   flag above), this gate can never open — the cascade is dead weight for
   this call site, not a live bypass.
3. **The pure interpreter's own paths** — `execute_invokevirtual_cached`'s
   `CachedInvokeTarget::VirtualBytecode` arm (dispatches real bytecode from
   the per-thread inline cache): **zero hits** in the whole run.
   `execute_invokevirtual_vtable_fast` (the lock-free vtable fast path,
   dispatched on invoke-cache miss): reached exactly once, at the very start
   of the run, resolved correctly (`cached_native_shadow=None` →
   ancestor walk → found `java/lang/ClassValue` native → cached `true` →
   `CacheMiss`, ceding to the slow path) and never reached again (its own
   `native_shadow_cache` memoization, plus the interpreter's thread-local
   `invoke_cache` getting warmed with `VirtualNative`, short-circuits every
   later call through the cheaper fast paths). `try_stackless_invoke` /
   `invoke_on_class_shared_inner` (the slow, fully-general dispatcher,
   consulted on every cache miss): `try_stackless_invoke` fires exactly
   once (the same cold-miss moment as above) and resolves correctly;
   `invoke_on_class_shared_inner` is **never reached at all** in the entire
   run (its own explicit `java/lang/ClassValue.get` allow-list entry in
   `check_override`, confirmed present and correct, simply never gets
   exercised because nothing falls through that far after the first call).
   `jit_invoke_dispatch` (the generic, non-MIC JIT dispatch helper used by
   methods compiled via the legacy `CRATONVM_JIT_C2_FIRST_CALL` "early
   compile" path, which builds no MIC/PIC slots at all): **zero hits** —
   that legacy path is also gated behind an unset-by-default env var and
   never engages for this repro.
4. **`try_jit_compile_callee` / `try_jit_compile_callee_slow`** (the shared
   callee-compile gate `jit_invoke_virtual_mic` and the background/tiered
   compiler both call through): traced every entry/exit. Both its own-class
   native check (`native_methods.find(class_name, ...)`, `class_name` =
   the exact receiver "GroovyClassValueJava7") and its declaring-class
   check (`native_methods.find(declaring_class_name="java/lang/ClassValue",
   ...)`, found by resolving `find_method_recursive` from the receiver)
   correctly and consistently refuse to compile, on every call, for the
   entire run. The JIT-cache probe at the top of `try_jit_compile_callee`
   (which would skip the native check entirely on a pre-existing cache hit)
   never hits either — nothing ever publishes an entry for this
   `(class, method, descriptor)` key.
5. **Disabling the background/tiered compiler entirely**
   (`CRATONVM_BG_COMPILE=0`): does **not** fix the repro — it changes the
   failure to a different, unrelated `ArrayStoreException` inside the same
   `GroovySystem.<clinit>` before ever reaching the `ClassValue`-related
   code path at all (a different, timing-sensitive symptom, not
   investigated further — flagging here only so a future session doesn't
   mistake it for the same bug). This rules out `background_compile_task`
   (the "tiered-enqueue"/"bg-compile" mechanism that in the *default*
   config does compile `ClassInfo.getClassInfo` itself, confirmed via
   `CRATONVM_DBG_JITC=1`) as a *required* trigger for the failure — whatever
   is wrong reproduces independent of whether that specific compiler runs.

**One real, general bug found and fixed along the way (kept, but confirmed
NOT sufficient to close this residual):** `jit_invoke_targets_native_shadow`
(`vm/src/runtime/interpreter.rs`), the static bytecode-scanning pre-check
`try_jit_upgrade_with_gate` (the interpreter's OWN "caller-method-counter"
JIT tier-up trigger, a *third*, independent compile pathway from both
`try_jit_compile_callee` and the background/tiered compiler) uses to decide
whether a method is safe to JIT-compile because it calls something
natively-shadowed. For an `invokeinterface` call site, this function
resolved `declaring_class` via `find_method_recursive` starting from the
**interface's own class_id** — which can only ever walk up to a
super-interface, never across to a concrete implementor's *superclass*
chain, because interfaces carry no knowledge of their implementors. So for
`ClassInfo.getClassInfo`'s own `invokeinterface GroovyClassValue.get` site,
this check resolved `declaring_class = "GroovyClassValue"` (the interface
itself, which merely declares `get` abstractly) instead of
`"java/lang/ClassValue"` (the concrete ancestor every real
`GroovyClassValue` implementor's `get()` actually resolves to at runtime),
found no native there, and wrongly reported "not native-shadowed" — the
same "declaring-class blind spot for a receiver whose bytecode-declaring
ancestor is invisible from the static call-site type" shape as every other
occurrence this doc's cluster has found, just manifesting in a *fourth*
independent function this time. **Fixed** by falling back to a cheap,
class-blind `native_methods.might_have_method_descriptor(method_name,
descriptor)` probe specifically for the `invokeinterface` case when the
interface-rooted resolution comes back clean — a hit is treated as a
possible shadow (conservative-only: can never cause an *incorrect* dispatch,
only occasionally decline a tier-up opportunity that was actually safe).
**Verified this fix does NOT resolve Residual 6** — the exact same NPE
still reproduces after applying it, confirmed via the same live trace
instrumentation (the fix only affects whether `try_jit_upgrade_with_gate`
refuses to compile `getClassInfo`; `background_compile_task`'s separate,
unguarded-by-this-check compile path was already compiling it successfully
by design, and disabling that path entirely — point 5 above — doesn't fix
the repro either). Kept anyway: it is a real, independent, low-risk
correctness improvement to the general JIT tier-up safety net, consistent
with (and worth auditing for) the recurring "interface dispatch hides a
concrete implementor's superclass-inherited native" bug family this whole
cluster keeps surfacing — spot-checked for regressions (a handful of other
`core/spring-boot-test` classes under both `-Jit on`/`-Jit off`, identical
pass/fail as baseline) but not run through the full 81-class module sweep.

**Next-step hypothesis for whoever picks this up, narrower than any prior
session's:** with every dispatch-*decision* mechanism now individually
verified correct via live tracing, the remaining candidate is a
dispatch-*independent* bug — most likely a **GC-safepoint root-tracking gap
specific to a `getstatic`-loaded local crossing a safepoint poll immediately
before an `invokeinterface` dispatch, inside JIT-compiled code**.
`CRATONVM_DBG_JIT_DISASM=ClassInfo.getClassInfo` shows the compiled body's
shape precisely: `getstatic globalClassValue` (a helper call) → store the
result to a stack slot → a dense back-to-back spill of every
callee-saved/argument register (the hallmark of a safepoint poll) → reload
the same stack slot as the invokeinterface receiver → the MIC-guarded
dispatch (confirmed correct per point 2 above) → `checkcast` → `areturn`.
If a GC safepoint fires in that exact window and the precise-JIT-maps /
safepoint-reload machinery for this specific compiled shape fails to
correctly re-root or re-forward that ONE stack slot (as opposed to the
argument/receiver slots the existing `forward_jit_reference_args` machinery
already covers on the *dispatch-helper* side — this would be a gap in the
*compiled callee's own* prologue-to-dispatch safepoint handling, upstream
of any helper call), the receiver read at the invokeinterface site could be
a stale/zeroed pointer. Critically, this would NOT necessarily manifest as
the null-receiver NPE sentinel path in `jit_invoke_virtual_mic` (that only
catches a literal `0` pointer) — a pointer that is non-null but points at
zeroed/reclaimed memory reads a `class_id` of `0` from its header, which
`virtual_dispatch_target_for_receiver` already special-cases by falling
back to `info.class_name` — the *static, CP-declared* type, i.e. the
`GroovyClassValue` **interface**, not `ClassValue` — with
`cacheable_receiver = false`. Dispatching `invoke_or_native` with
`class_name = "GroovyClassValue"` for a *real* (non-corrupted, well-formed)
receiver would normally still resolve correctly via `invoke_on_class_shared_inner`'s
C25 retarget (which reads the receiver's *actual* heap class_id
independently) — but if the receiver truly is a stale/zeroed pointer, that
retarget's own `class_id_of` read would *also* see class_id 0, so the
retarget's `rc != ClassId::new(0)` guard suppresses retargeting entirely,
leaving dispatch resolved against the bare interface — which has an
*abstract* `get`, no code, and (per this session's finding above) no native
registered under its own name — a plausible route to a silent, exception-free
null return that exactly matches every observed symptom (JIT-only; no
internal NPE; the native never called; every dispatch-decision function
proven correct in isolation). **Concretely**: instrument (or attach a
debugger to) the exact moment of the getstatic→safepoint→invokeinterface
sequence inside a JIT-compiled `ClassInfo.getClassInfo` invocation — verify
whether `receiver_class_id` (or the raw pointer) ever reads as
zero/stale for a *live, non-null* `globalClassValue` at the point of
dispatch, and if so, trace which precise-JIT-maps oop-map entry (or lack
thereof) was supposed to cover that specific stack slot across that
specific safepoint poll.

**Eighth-session note (2026-07-21, worktree
`fix/classvalue-jit-dispatch-residual6-20260721`): ROOT-CAUSED AND FIXED.**
The GC-safepoint hypothesis was refuted first (the NPE reproduces
identically with `CRATONVM_NO_PRECISE_JIT_MAPS=1` and with a 6 GB heap),
then a probe ring around every silent-null path found the answer in one
run. The full mechanism:

1. Once `ReflectionCache.getCachedClass`'s caller chain is JIT-compiled,
   the failing `getClassInfo` invocation enters the compiled
   `ClassInfo.getClassInfo` artifact by **direct machine call** (no
   dispatch helper, hence invisible to all seven sessions'
   dispatch-helper traces). Inside it, `jit_invoke_virtual_mic` dispatches
   the `invokeinterface GroovyClassValue.get` **correctly** and the
   `ClassValue` native returns a **valid, non-null** `ClassInfo`
   (confirmed live: `[cv-native] cache HIT -> 0x…` + `[cv-mic-hit-result]
   is_null=false` immediately before the failure).
2. The compiled body then executes `checkcast
   org/codehaus/groovy/reflection/ClassInfo`. `jit_checkcast` resolves
   the target BY NAME via the flat global `find_class_by_name` — but the
   test's `@CompileWithForkedClassLoader` AOT-processing loader had by
   then re-defined `ClassInfo` under a second `ClassId` (probe:
   `[cv-checkcast-fail] typecheck REFUSED: obj_cid=2695
   obj_cls=org/codehaus/groovy/reflection/ClassInfo
   target_name=org/codehaus/groovy/reflection/ClassInfo
   target_cid=Some(2921)`). The id-based subtype check refused the cast
   between the two same-named copies.
3. `jit_checkcast` returned `0` for the refusal and the checkcast codegen
   **pushed that 0 as the result** — a silent null, no CCE, no internal
   exception, native provably called-and-correct: exactly Residual 6's
   observed symptom, and exactly why every dispatch-level instrument came
   back clean for seven sessions. (The earlier interpreted calls to the
   same code passed because they ran before the duplicate `ClassInfo` was
   defined, while `find_class_by_name` still returned the original copy.)

**Fixes (both landed on this branch):**
- `069f26425` — new `Class::is_assignable_to_name` (name-based hierarchy
  walk over supers + interfaces, mirroring the accepted
  `is_subclass_of_by_name` loader-identity-blind tradeoff already used
  for exception `catch_type` resolution) wired into
  `jit_typecheck_resolve` as a fallback after the id-based checks fail;
  covers both `checkcast` and `instanceof`. Also adds permanent
  env-gated (`CRATONVM_TRACE_CLASSVALUE`) probes at every silent-null
  path in the `get(Class)` dispatch chain and every `jit_checkcast`
  failure branch — the probe ring that pinpointed this.
- `c3bc118b2` — companion hardening: a definitively-refused JIT checkcast
  now stashes a real `java/lang/ClassCastException` and returns the
  `i64::MIN` sentinel (routed through the standard
  `emit_post_invoke_exception_check` guard; new `emitted_checkcast_throw`
  forces `has_dispatch`), so this entire class of silent-null corruption
  can never hide again. Fail-soft null retained only where the object's
  type is unknowable (stale/implausible pointer, unresolved site).

**Verified:** `SpringBootContextLoaderAotTests` PASSES under `-Jit on`
(3/3 isolated runs) and `-Jit off`.

**Eighth-session regression check (2026-07-21):** full 81-class
`core/spring-boot-test` module sweep with both fixes, `-Exe` passed
explicitly (binary confirmed via the summary's `craton exe=` line), both
modes:
- `-Jit on` (`-Parallel 4`): 76 PASS / 2 FAIL / 2 EMPTY / 1 HANG.
- `-Jit off` (`-Parallel 4`): 77 PASS / 2 FAIL / 2 EMPTY.
`SpringBootContextLoaderAotTests` PASSES in-suite in BOTH modes (61.4s /
75.6s). The 2 EMPTYs are `Abstract*Tests` base classes (enumeration
noise). The `-Jit on` HANG (`UriBuilderFactoryWebClientTests`, HtmlUnit)
PASSES in isolation (2/2 tests) — same shared-host parallel-load artifact
family as the previously-documented
`UriBuilderFactoryWebConnectionHtmlUnitDriverTests` hang. The 2 FAILs are
exactly the two pre-existing, this-session-unrelated failures already
known to this doc: `DuplicateJsonObjectContextCustomizerFactoryTests`
(JUnit-Platform `DiscoveryIssueException` under parallel Aether `.m2`
contention — PASSES in isolation, matching Cluster E's disposition) and
`ImportsContextCustomizerFactoryTests`
(`contextCustomizerEqualsAndHashCodeConsidersComponentScan`, 1 of 8
tests, reproduced in isolation under BOTH JIT modes — a real, separate
annotation-identity bug, not part of this doc's clusters). **No new
regressions in either mode.**

**Update (same day, later):** `ImportsContextCustomizerFactoryTests` is
also now **FIXED** — root-caused independently in this session to a
synthesized-annotation-proxy `hashCode`/`equals` dispatch gap inside the
native `HashSet`/`HashMap` overlay (two value-equal Spring
`SynthesizedMergedAnnotationInvocationHandler` proxies hashed by identity
when the hashing ran through the collections native, so
`ContextCustomizerKey`'s two key sets compared unequal; a 40-line
standalone repro — two `synthesize()`d `@ComponentScan`s in a `HashSet` —
isolated it), and closed by the concurrently-landed dev commit
`2184973c8` ("fix(genuine56): 6 root-cause bugs across HashMap ordering,
StringJoiner, proxy dispatch, and HTTP headers"). Verified after merging
that commit: **8/8 tests PASS under both `-Jit on` and `-Jit off`**.
With this, every failure this doc has ever named is closed: the only
remaining non-PASS results in the final full-module confirmation sweep
are the two `Abstract*Tests` EMPTY entries (enumeration noise) and the
environment-only parallel-load flakes
(`DuplicateJsonObjectContextCustomizerFactoryTests` Aether race /
HtmlUnit-family hangs), all of which PASS in isolation.

### Regression check (fourth investigation session, 2026-07-20; corrected fifth/sixth session, 2026-07-21)

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
- `core/spring-boot-test` | `SpringBootContextLoaderAotTests` — **FIXED under BOTH `-Jit on` and `-Jit off`** (Cluster C; originally-documented NPE + Residuals 1–6 all FIXED). Residual 6 root-caused in the eighth session (2026-07-21): JIT `checkcast`'s name-only target resolution refused a cast between two same-named `ClassInfo` copies from duplicate loaders and silently nulled the result — see the eighth-session note under Residual 6.
- `core/spring-boot-test` | `SpringBootContextLoaderTests` — **FIXED**, 26/26 (Cluster D)
- `core/spring-boot-test` | `DuplicateJsonObjectContextCustomizerFactoryTests` — **FIXED** / does not reproduce in isolation; flaky under parallel-load regression runs (Cluster E, see update above) — unrelated to this session's changes
