# CratonVM Spring suite — genuine bug list (dev `8719dca85`)

| | |
|---|---|
| **Status** | OPEN — 35 confirmed genuine bugs remaining (6 more fixed 2026-07-21 FactoryBean/CGLIB session — 3 methods across Spr6602Tests/Spr15275Tests plus 4 `ConfigurationClassPostProcessorTests` methods, see below; AOT cluster excluded, being worked separately) |
| **Captured** | 2026-07-17 (initial full-suite triage, dev `213d93ea`), reconfirmed 2026-07-20 (dev `8719dca85`), 15 more fixed 2026-07-21 (dev `693702a2a`), 1 more fixed 2026-07-21 late session (dev `063cd747d`), 7 more fixed 2026-07-21 FactoryBean/CGLIB session (see below) |
| **Worktree** | `/data/wt-spring-full-suite-20260717` (branch `chore/spring-full-suite-20260717`), Azure host `20.83.144.174`; this session's fix landed from a local Windows checkout, verified against `apps/spring-framework` directly (no Azure host needed) |

## 2026-07-21 FactoryBean-enhancement + general AOP-CGLIB-of-native-class session

Scope: the two open items this doc flagged as needing "substantial new
native-VM feature work" — the FactoryBean-enhancement proxy generator
(`Spr6602Tests`/`Spr15275Tests`) and the general AOP-CGLIB-of-native-class
gap (`ConfigurationClassPostProcessorTests.genericsBasedInjectionWith*`).
**Both fully fixed**, on top of the existing `native-builtins/src/
cglib_enhancer.rs` `ConfigurationClassEnhancer.enhance` native
reimplementation. All changes landed in `native-builtins/src/
cglib_enhancer.rs`, `native-builtins/src/classloader.rs`, `native-builtins/
src/lang_class.rs`, and `classloading/src/class_manager.rs`.

**1. FactoryBean-enhancement proxy generator — FIXED (3 methods, 2
classes).** Implemented the missing piece this doc's earlier session had
root-caused but deferred: `emit_bean_override`'s inter-bean-reference path,
for a FactoryBean-typed `@Bean` method, now splices an extra `invokestatic
cratonvm/internal/ConfigEnhancerSupport.enhanceFactoryBeanReference(Object
rawFactory, Object beanFactory, String beanName, String
exposedTypeInternalName)Object` call between the raw `getBean("&name")`
lookup and the result checkcast (8 extra bytes, exception-table absolute
offsets shifted accordingly — regression-guarded by a new unit test,
`fb_ref_splice_shifts_exception_table_by_exactly_8_bytes`, that parses the
generated class with the real `cratonvm-reader` crate and checks the byte
math independently of any VM run). The new Rust helper mirrors real
Spring's `ConfigurationClassEnhancer$BeanMethodInterceptor
.enhanceFactoryBean` exactly:
- If the raw factory's *runtime* class (or its `getObject()` override) is
  `final` and the `@Bean` method's *declared* return type is an interface:
  builds a real JDK dynamic proxy (reusing this VM's existing
  `define_or_get_proxy_class` real-`$ProxyN`-class machinery, the same
  path `wrap_annotation_in_real_proxy` uses) implementing just that one
  interface. Its `InvocationHandler` (`cratonvm/internal
  /FactoryBeanEnhancerHandler`, dispatched via a plain native registered on
  `(invoke, ...)` — picked up automatically by the existing
  `invoke_or_native` proxy-dispatch path, no VM-side special-casing needed)
  intercepts only `getObject()` (delegating to `beanFactory.getBean(name)`,
  resolving the container's cached product) and forwards every other call
  straight to the raw factory via real virtual dispatch.
- Otherwise: builds a field-copying CGLIB-style subclass of the raw
  factory's own concrete class (cached per concrete class, like the
  existing `ConfigurationClassEnhancer` proxy cache), instantiated via
  `ctx.allocate_instance` (no `<init>` call — sidesteps arbitrary/absent
  constructor descriptors, the same Objenesis-style trick real CGLIB
  uses) with every inherited instance field copied from the original by
  native `get_field`/`set_field` (using each field's already-known heap
  slot index — identical on both objects since the wrapper only appends
  its own two new fields after the inherited ones). Only `getObject()`
  (every distinct declared signature — the covariant override and any
  compiler-synthesized generics bridge) is overridden to delegate to
  `beanFactory.getBean(name)`; everything else runs the real inherited
  body unchanged.
- All GC-unsafe sequences (an allocation between reading and using a
  pinned `ObjectRef`) are wrapped in `pin_native_root`/`read_native_pin`/
  `unpin_native_roots`, mirroring `wrap_annotation_in_real_proxy`'s own
  idiom.

One intermediate bug found and fixed during verification: the bytecode
splice initially passed the SAME `beanName` local used for the raw-factory
`getBean("&name")` lookup into the new helper — always "&"-prefixed for a
FactoryBean-typed method — causing the wrapper's `getObject()` to resolve
the raw factory a second time (via `getBean("&name")`) instead of the
product (`getBean("name")`), which then failed its checkcast to the
product type (e.g. `BarFactory cannot be cast to Bar`). Fixed by stripping
the leading `&` in `enhance_factory_bean_reference` before use.

Verified (Windows, real JDK 25, release build): `Spr6602Tests` 2/2 (was
1/2), `Spr15275Tests` 6/6 (was 4/6 — both previously-failing methods,
`withFactoryBean` and `withFinalFactoryBean`, exercise the interface-proxy
path; the CGLIB-subclass path is exercised by `Spr6602Tests`'s
`barFactory()`/`foo()` inter-bean reference). No regressions across a
broader sweep: `ConfigurationClassPostConstructAndAutowiringTests` 2/2,
`ConfigurationWithFactoryBeanAndAutowiringTests` 7/7,
`ConfigurationWithFactoryBeanAndParametersTests` 1/1,
`ConfigurationWithFactoryBeanEarlyDeductionTests` 11/11,
`AnnotationConfigApplicationContextTests` 35/35,
`ConfigurationClassAndBeanMethodTests` 3/3,
`ConfigurationClassAndBFPPTests` 3/3, `ConfigurationClassWithConditionTests`
14/14. `cargo test -p cratonvm-native-builtins --lib`: 3056 passed / 0
failed / 6 ignored (full suite, no pre-existing-failure caveats needed this
time).

**2. General AOP-CGLIB-of-native-class support — FIXED (4 methods, the
full `genericsBasedInjectionWith{Early,Late}GenericsMatchingOn{Cglib,
Jdk}Proxy` cluster).** This doc's prior session hypothesized the root cause
was "native-class bytecode retrievability" — classes defined via
`define_class_full` not being retrievable via `getResourceAsStream` for
real CGLIB's ASM `ClassReader` to introspect. **That hypothesis was
investigated and implemented (see item 3 below, an independently valid VM
improvement) but did NOT fix this cluster** — the real root cause,
confirmed this session via iterative bisection with temporary `eprintln!`
diagnostics (all removed except a few concise, permanently env-gated
`CRATONVM_DBG_FBCGLIB` ones matching this codebase's existing convention),
is a **class-naming collision**, not a bytecode-retrieval gap:

- Real Spring's `CglibAopProxy` (`proxyTargetClass=true` general AOP
  proxying), before building its own CGLIB subclass, checks
  `ClassUtils.isCglibProxyClass(rootClass)` (true for any class name
  containing `"$$"`) and, if true, unwraps to `rootClass.getSuperclass()`
  — deliberately avoiding double-proxying an already-CGLIB-proxied class.
  Since our native `ConfigurationClassEnhancer.enhance()` reimplementation
  names its generated class exactly like a real CGLIB proxy
  (`<Original>$$SpringCGLIB$$<n>`, matching Spring's own
  `SpringNamingPolicy` convention, deliberately — other already-fixed
  tests hardcode this exact name), `isCglibProxyClass` correctly treats it
  as one and unwraps to the ORIGINAL, plain `<Original>` class.
- Real cglib then computes its OWN "first CGLIB proxy of `<Original>`"
  name using the identical `SpringNamingPolicy` convention — landing on
  the EXACT SAME string, `<Original>$$SpringCGLIB$$0`, our own native
  reimplementation already used for the first-level enhancement. On real
  HotSpot this never collides, because BOTH enhancements go through real
  cglib's own Java-level naming bookkeeping (`AbstractClassGenerator
  .ClassLoaderData`'s reserved-names set), so the second one detects the
  name is taken and bumps its own counter. Our first-level enhancement
  bypasses that bookkeeping entirely (it's a native reimplementation, not
  real cglib bytecode), so real cglib never learns the name is taken.
- Real cglib's `defineClass` attempt for its own (real, fully-featured,
  ~12-13KB) generated class under that already-taken name is correctly
  rejected by this VM's classloader (`IncompatibleClassChangeError` /
  `ClassFormatError`, a `LinkageError` subtype) — matching what a genuine
  same-name race would do on any JVM. Real cglib's own defineClass-failure
  recovery path then loads OUR existing (much smaller, ~3.9KB) native
  reimplementation class instead, treating it as if it were the class it
  just "generated". That recovery path (`Enhancer.wrapCachedClass` and
  related `AbstractClassGenerator`/`Enhancer` bookkeeping) needs several
  cglib-internal artifacts real cglib-generated classes always carry,
  which our minimal reimplementation never had: five `public static`
  fields (`CGLIB$FACTORY_DATA`, `CGLIB$CALLBACK_FILTER`,
  `CGLIB$THREAD_CALLBACKS`, `CGLIB$STATIC_CALLBACKS`, `CGLIB$BOUND` — all
  `Object`-typed except the last, `boolean`) and the
  `org/springframework/cglib/proxy/Factory` marker interface (with all
  seven of its methods — `newInstance` ×3 overloads, `getCallback`,
  `setCallback`, `getCallbacks`, `setCallbacks` — implemented as harmless
  null-returning/no-op stubs, since nothing in this VM's own dispatch ever
  calls them; they exist purely so an incidental cast/reflection from
  OTHER real cglib machinery that mistakes this class for its own doesn't
  throw). All now emitted unconditionally by `build_enhancer_class` for
  every generated `@Configuration` enhancer class, regression-guarded by
  two new unit tests (`enhancer_class_declares_all_cglib_bookkeeping_fields`,
  checking field names/descriptors/access flags, and an extension of the
  same test checking the `Factory` interface + its 7 method signatures).

Bisection method (useful precedent for future similar investigations):
`KRUN_STACK=1` on the failing test class surfaced the FULL cause chain
(`AopConfigException: Could not generate CGLIB subclass...` →
`CodeGenerationException: java.lang.NoSuchFieldException-->CGLIB$
FACTORY_DATA` → `NoSuchFieldException: CGLIB$FACTORY_DATA`) — Spring's own
`CglibAopProxy.getProxy()` catch-all wrapper collapses ANY
`CodeGenerationException` into the generic "final class or non-visible
class" message, so the real cause is invisible without `getCause()`
chasing. Each missing artifact, once added, changed the failure to the
NEXT missing one (`CGLIB$FACTORY_DATA` → `CGLIB$CALLBACK_FILTER` →
`ClassCastException: ... cannot be cast to
org.springframework.cglib.proxy.Factory`) rather than fixing it outright —
worth expecting this "peel one layer at a time" pattern if this same
mechanism resurfaces elsewhere (e.g. the `withFinalFactoryBeanAsReturnType`
family, or other `@CompileWithForkedClassLoader`/general-AOP-on-native-
class scenarios not yet exercised by this suite).

Verified (Windows, real JDK 25, release build, deterministic across
repeat runs): `ConfigurationClassPostProcessorTests` 80/85 (was 74/85 —
all 4 `genericsBasedInjectionWith*` methods now pass; 0 remaining
failures are CGLIB-generation related). The 5 residual failures are
unrelated, pre-existing issues, confirmed independently:
`configurationClassesWithInvalidOverridingForProgrammaticCall` (this doc's
own already-documented checkcast/message-format gap, see below),
`beanDefinitionsFromBeanMethodWith{BeanNameGenerator,
ConfigurationBeanNameGenerator}` (both fail identically with
`IllegalStateException: Could not initialize plugin: interface
org.mockito.plugins.MockMaker` — a Mockito-inline-mock-maker
service-file/environment gap, not investigated this session),
`nullArgumentThroughBeanMethodCall` ("No BarArgument injected") and
`beanLookupFromSameConfigurationClass` (`NoSuchMethodException:
getTestBean`) — neither investigated this session, not cglib-generation
shaped. No regressions: `ConfigurationClassEnhancerTests` 4/5 unchanged
before/after (the one failure, `withPublicClass`, confirmed via `git
stash` to fail identically on the pre-session baseline — a pre-existing
gap where `config_enhancer_class_cache` is keyed only by the original
class's `ClassId`, not `(ClassId, loader)`, so `enhance()`'s SAME source
class through a SECOND, DIFFERENT `ClassLoader` incorrectly reuses the
first loader's cached result instead of regenerating — not touched this
session, flagged for a future fix).

**3. Native-class bytecode retrievability — implemented as a general VM
improvement, independently useful even though it did not turn out to be
this cluster's root cause.** `ClassLoader.getResourceAsStream`/`Class
.getResourceAsStream` previously could never serve a `.class` resource
for a dynamically-*defined* class (`Unsafe.defineClass`/`Lookup
.defineClass`/`ConfigurationClassEnhancer`'s own `define_class_full` calls
included) — `ClassManager::find_resource` only ever searched the
bootstrap/extension/application classpath, never the `class_bytes_cache`
every defined class's raw bytes are already cached in. Fixed via a new
`defined_class_resource_bytes` helper in `native-builtins/src/
classloader.rs`'s `cl_get_resource_as_stream`, resolving a `".class"`
resource request through the existing `ctx.class_id_by_name` +
`ctx.class_bytes` trait methods before falling back to the classpath scan
— loader-blind (same caveat as `resolve_or_load_class_id` elsewhere in
this codebase), but safe in practice since every class this can reach was
named by one of this crate's own generators with a process-globally-unique
counter suffix, so no two loaders ever collide on the name. Kept in this
session's commit as a real, tested (regression suite green) improvement
for whatever DOES eventually need it (real ASM/bytecode-introspection
tooling reading back a runtime-generated class's own bytecode), documented
here so a future session doesn't need to re-derive it, but should NOT be
assumed to fix any currently-open item in this doc — item 2 above was the
actual mechanism for the `genericsBasedInjectionWith*` cluster.

## 2026-07-21 late session — non-AOT residual sweep

Scope: every OPEN class in this doc EXCLUDING the AOT cluster (both the
strict `*.aot.*`-package classes and the wider set of TIMEOUT classes swept
into that investigation's narrative — `core.io.buffer.DataBufferTests`,
`scripting.groovy.GroovyScriptFactoryTests`,
`context.groovy.GroovyBeanDefinitionReaderTests`,
`context.annotation.ComponentScanParserBeanDefinitionDefaultsTests`,
`test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests`,
`web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests`/
`RequestMappingMessageConversionIntegrationTests`, and
`web.service.registry.*` — left alone per explicit instruction, another
session (`wt-aot-cluster-20260721`, branch `fix/aot-cluster-residuals-20260721`)
is actively working that cluster). Worktree
`/data/wt-spring-genuine-residuals-20260721` (branch
`fix/spring-genuine-buglist-residuals-20260721`), binary
`cratonvm-springresid-v2.bin`.

**1 genuine bug fixed.** `native-builtins/src/cglib_enhancer.rs`'s
`emit_bean_override` (the native `ConfigurationClassEnhancer` `@Bean`-method
proxy-override bytecode emitter) implemented the
`isCurrentlyInvokedFactoryMethod` → `super.<name>()` /
inter-bean-reference → `getBean(name)` dichotomy from real Spring's
`BeanMethodInterceptor.intercept`, but never replicated
`resolveBeanReference`'s SPR-8080 reentrancy guard: temporarily clearing
`ConfigurableBeanFactory.setCurrentlyInCreation(beanName, false)` around the
inter-bean `getBean()` call (restored in a `finally`) whenever that bean
name was already marked "currently in creation" by an enclosing
`getSingleton()` further up the call stack. Without it, a `@PostConstruct`
method on a `@Configuration` class that called one of its own sibling
`@Bean` methods — when that factory bean's OWN creation was triggered as a
side effect of resolving the `@Bean` method's product as a dependency
elsewhere (e.g. `Config2` registered before `Config1`, so `Config1` gets
created while the container is mid-`getSingleton("beanMethod", ...)`) —
tripped a spurious `BeanCurrentlyInCreationException` that real Spring
resolves fine. Fixed by adding the same temporarily-clear/finally-restore
dance as hand-written JVM bytecode (new locals for `alreadyInCreation`/`cbf`/
the caught throwable, one new exception-table entry covering just the
`getBean()` call, mirroring `resolveBeanReference`'s `try/finally` exactly).
Fixes `context.annotation.ConfigurationClassPostConstructAndAutowiringTests`
.`originalReproCase` (2/2, was 1/2). No regression: reran
`ConfigurationClassPostProcessorTests`/`Spr15275Tests`/`Spr6602Tests` and a
22-class `context.annotation.*Configuration*` sweep (all unchanged vs.
pre-fix baseline), plus `cargo test -p cratonvm-vm --lib --release`
(2227 passed / 10 failed — 8 pre-existing lock_order release-mode-only
"should panic" assertions plus 2 confirmed-pre-existing/flaky via a
git-stash A/B: `enforcement_active_in_debug_builds` fails deterministically
in ANY release build regardless of code changes (it literally asserts
`cfg!(debug_assertions)`), `native::jni::tests::process_vm_publish_and_resolve`
passed in isolation and is unrelated to this crate — both confirmed
unaffected by this fix). Landed on `dev` at `063cd747d`
(`fix/spring-genuine-buglist-residuals-20260721`).

**3 classes confirmed already fixed as side effects of other concurrent
dev work** (no code change needed, stable across repeat runs):
`web.context.request.RequestScopeTests` (0/7 → 7/7),
`web.servlet.view.groovy.GroovyMarkupViewTests` (9/10 → 10/10). Also
`web.client.RestClientIntegrationTests` (226/230 → 227/230, 2 fail + 1
abort remain), `web.client.RestTemplateIntegrationTests` (118/125 → 119/125,
3 fail + 3 abort remain), and `web.reactive.function.client
.WebClientIntegrationTests` (168/170 unchanged, now 1 fail + 1 skip)
improved but did not fully clear — see residuals below.

**Root-caused but NOT fixed this session** (each would need substantial new
native-VM feature work or live/gdb tracing beyond this session's time
budget — flagged for a dedicated follow-up):

- **`context.annotation.Spr6602Tests`.`configurationClassBehavior` +
  `context.annotation.Spr15275Tests`.`withFactoryBean`/`withFinalFactoryBean`
  (3 methods, 2 classes). FIXED 2026-07-21 — see "FactoryBean-enhancement +
  general AOP-CGLIB-of-native-class session" above for the implementation;
  the root-cause analysis below is kept for historical context.** Real
  Spring's `ConfigurationClassEnhancer
  .BeanMethodInterceptor.enhanceFactoryBean()` — when an inter-bean
  reference resolves to a `FactoryBean`, wraps it in a CGLIB subclass (or a
  JDK interface proxy, for a `final` factory exposed via an interface
  return type) whose `getObject()` delegates to the container's cached
  product (`beanFactory.getBean(name)`) instead of the factory's real
  `getObject()` body — has no native-reimplementation equivalent here;
  `emit_bean_override` only ever does the plain `getBean(&name)`/
  `getBean(name)` dichotomy (already correctly chooses `&name` for
  FactoryBean-typed methods, just returns the RAW factory instead of an
  enhanced one), so a raw `factory.getObject()` call bypasses the
  container's `factoryBeanObjectCache` entirely, returning a fresh
  (non-singleton-matching) product instance. Confirmed via
  `Spr6602Tests`'s exact failure (`bar1` from the container's cache !=
  `foo.bar` from the raw uncached `getObject()` call). A full fix needs:
  (1) a new dynamic-subclass-or-interface-proxy generator reusing this
  file's `ClassWriter`; (2) `ctx.allocate_instance(name)` +
  `ctx.set_field_by_name(...)` to sidestep constructor-descriptor/
  anonymous-class-outer-instance-capture problems entirely (no `<init>`
  call needed at all — same trick real CGLIB's Objenesis path uses); (3) a
  new `invokestatic` dispatch target from the generated bytecode into this
  new Rust helper. **(3) was the main open uncertainty and is now
  RESOLVED**: `ctx.ensure_synthetic_class(...)` (see
  `native-builtins/src/lang_system.rs`'s `cratonvm/internal/UnmodifiableMap`
  for a working precedent) confirms purely-synthetic invokestatic targets
  with no real `.class` bytes ARE supported by this VM, so a
  `cratonvm/internal/ConfigEnhancerSupport.enhanceFactoryBeanReference(...)`
  -style helper is a safe, proven pattern here — just not implemented.
- **`context.annotation.ConfigurationClassPostProcessorTests`** (11/85
  fail, unchanged from this session's own pre-fix baseline — note this is
  DOWN from the doc's previously-recorded 82/85/3-fail state, i.e. 8 MORE
  failures appeared here between 2026-07-21's earlier session and this one,
  from unrelated concurrent dev work landing on `dev` in between; not
  investigated). Two distinct root causes found for 5/11:
  - 4 failures (`genericsBasedInjectionWith{Early,Late}GenericsMatchingOn
    {Cglib,Jdk}Proxy`). **FIXED 2026-07-21** — see "FactoryBean-enhancement
    + general AOP-CGLIB-of-native-class session" above; the actual root
    cause turned out to be a class-naming collision + missing cglib
    bookkeeping artifacts, NOT the bytecode-retrievability hypothesis
    below (that hypothesis was still implemented as an independent VM
    improvement, see item 3 in that session's write-up, but was not what
    fixed this cluster). Kept for historical context: Spring AOP's
    `proxyTargetClass=true` auto-proxy
    creator tries to CGLIB-subclass an ALREADY-native-CGLIB-generated
    `ConfigurationClassEnhancer` proxy class
    (`RepositoryConfiguration$$SpringCGLIB$$0`); real CGLIB bytecode-gen
    (there's no native AOP-CGLIB-proxy reimplementation anywhere in
    `native-builtins`, unlike `ConfigurationClassEnhancer.enhance` — general
    AOP CGLIB subclassing of ordinary classes must therefore be working via
    REAL CGLIB bytecode execution today) then fails with cglib's own
    generic `Could not generate CGLIB subclass... Common causes of this
    problem include using a final class or a non-visible class`. Most
    likely cause: classes defined via `define_class_full` (this file's
    `build_enhancer_class`) don't expose retrievable `.class` bytes via
    `getResourceAsStream`/similar for real CGLIB's ASM-based
    `ClassReader` to introspect when asked to subclass one of them a
    SECOND time. This is a general VM-level gap (native-class bytecode
    retrievability for reflective/ASM tooling), not specific to this file
    — needs investigation in `classloader.rs`/`classloader_real.rs`.
  - 1 failure (`configurationClassesWithInvalidOverridingForProgrammaticCall`)
    — `emit_bean_override`'s inter-bean-reference path does a raw JVM
    `checkcast <Ret>` after `getBean()`, throwing a bare
    `ClassCastException` on type mismatch instead of replicating real
    Spring's `resolveBeanReference` `ClassUtils.isAssignableValue` check +
    descriptive `IllegalStateException` (`"@Bean method X.y called as bean
    reference for type [...] but overridden by non-compatible bean
    instance of type [...]. Overriding bean of same name declared in:
    ..."`). Full replacement bytecode designed in detail (instanceof+null
    check inside the existing SPR-8080 try-region, `StringBuilder` message
    build using a Rust-precomputed static prefix + `Class.getName()`/
    `Object.getClass()` reflective calls for the dynamic parts, throw
    `IllegalStateException`) but not implemented — mechanical, ~90 more
    bytes, all new constant-pool entries are straightforward reuses of
    patterns already in this file. This ALSO throws a customer-visible raw
    `ClassCastException` instead of Spring's real message ANYWHERE an
    inter-bean `@Bean` reference resolves to an incompatible override
    anywhere else in the suite — likely affects more than just this one
    test, worth fixing first in a follow-up.
  - Remaining 6/11 failures not investigated at all this session.
- **`jndi.JndiObjectFactoryBeanTests`.`lookupWithExposeAccessContext`**
  (24/25). Confirmed the exact expected math from real
  `JndiObjectFactoryBean`/`JndiObjectTargetSource`/
  `JndiContextExposingInterceptor` source: 1 `Context.close()` from
  `JndiObjectTargetSource.afterPropertiesSet()`'s eager `lookup()`, + 1 from
  the single ELIGIBLE proxied invocation (`setAge`, interface-declared).
  `equals()`/`hashCode()` should be short-circuited by `JdkDynamicAopProxy`
  before ever reaching the interceptor; `toString()` reaches it but
  `isEligible()` should return `false` since its `Method.getDeclaringClass()
  == Object.class`. CratonVM produces 3 closes (1 extra) — needs live/gdb
  tracing of the native `java.lang.reflect.Proxy` invocation-handler
  dispatch to find which of the three incorrectly gets routed through with
  a non-`Object` declaring class (or isn't fast-path short-circuited);
  static grep of `native-builtins` found no obvious culprit.
- **`orm.jpa.support.PersistenceInjectionTests`.
  `publicExtendedPersistenceContextSetterWithSerialization`** (26/27).
  `DummyInvocationHandler.closed` stays `false` after a `SimpleMapScope`
  Java-serialization round-trip + `serialized.close()`. Involves a
  scope-destruction-callback object (likely wrapping the
  `ExtendedEntityManagerCreator`-generated `EntityManager` proxy) needing
  to survive Java serialization and still correctly invoke `close()` post-
  deserialization — deep cross-cutting serialization+scope+JPA-proxy
  interaction, not traced to a specific native gap.
- **`test.context.bean.override.mockito.MockitoBeanByTypeLookupIntegrationTests`
  + the sibling `.constructor.MockitoBeanByTypeLookupForConstructorParametersIntegrationTests`**
  — **FIXED 2026-07-23 (5/5 and 6/6, both fully green).** Both classes went
  through five real bugs across two sessions on 2026-07-23, all variations on
  one theme: `Mockito.mock(StringBuilder.class)` (final class, inline mock
  maker) redefines `StringBuilder` AND its package-private superclass
  `AbstractStringBuilder` in place, and several different CratonVM
  caching/dispatch layers weren't redefine-aware.

  1. `Class.getDeclaredMethods()` mis-paired bridge methods to the FIRST
     same-named non-bridge sibling instead of the one it actually bridges
     (matched by parameter types only, not descriptor). Fixed in
     `native-builtins/src/lang_class.rs::native_class_get_declared_methods`
     (dev `45e2757d2`).
  2. `class_declares_method`'s ancestor walk (used by
     `native_mockito_mock_method_advice_is_overridden`) counted a
     compiler-generated bridge as a genuine "declared override", making
     `isOverridden(mock, AbstractStringBuilder#substring)` wrongly answer
     `true` — the woven advice's own preamble then skipped `handle()`
     entirely and ran the real (empty-buffer) computation. Fixed by
     excluding bridges in `vm/src/vm/vm_exec.rs::class_declares_method`
     (dev `f5379b3a0`).
  3. Two invoke-cache blind spots in `execute_invokevirtual_cached` /
     `populate_virtual_invoke_cache`, both keyed on "has this class ever
     been redefined" without checking it at the RIGHT layer for the plain
     `Native` cache-target variant. Fixed by extending the cache-hit guard
     to cover `Native` and adding a `receiver_redefined` check to the
     populate-side lookup.
  4. `is_string_builder_layout_native_override`'s "always native, real
     bytecode reads an incompatible compact-string layout" allowlist had two
     gaps: `setCharAt` was simply missing (a REAL StringBuilder used after
     an unrelated mock crashed with AIOOBE); `substring(int,int)` (2-arg,
     used internally by Mockito's own `StringUtil.join` when formatting an
     exception message) needed the same treatment, but the 1-arg overload
     had to stay OFF the list since that's the one these tests stub/verify.
     Fixed by giving the function the descriptor parameter.

     All four verified against HotSpot, `cargo test -p cratonvm-vm --lib`:
     2229 passed, 13 failed (pre-existing baseline, unrelated). Commit
     `0bd8213de`, merged `6a5f3db42`.

  5. **The residual that took both classes from 3/5 and 4/6 to 5/5 and 6/6**:
     `length()` had the exact same compiler-generated public-bridge shape as
     `substring(int)` (`AbstractStringBuilder` is package-private, so
     `StringBuilder.length()`/`StringBuffer.length()` are real,
     class-file-declared bridges — confirmed via `javap -p -c
     java.lang.StringBuilder`), but stayed on the blanket-immune allowlist
     for ALL THREE class names (`StringBuilder`, `StringBuffer`,
     `AbstractStringBuilder`) — the previous session's documented "KNOWN
     GAP". Root-caused this session by dumping Mockito's OWN redefined
     bytecode on real HotSpot (`-Dnet.bytebuddy.dump=...`, JDK 25, Mockito
     5.23.0): the woven `MockMethodDispatcher.get/isMocked/isOverridden/
     handle` advice is woven directly into `AbstractStringBuilder.length()`
     itself (NOT the `StringBuilder`/`StringBuffer` bridges, which stay
     unmodified plain delegation), so blanket-forcing native for
     `AbstractStringBuilder.length()` permanently pre-empted the advice for
     mock AND real receivers alike. A SEPARATE, previously-unnoticed
     redefine-unaware intrinsic-population code path in
     `populate_virtual_invoke_cache` (parallel to, but never updated
     alongside, the `execute_invokevirtual_vtable_fast` guard already fixed
     for bug 3) also needed the same redefine-awareness guard, since
     `StringBuilder.length()`'s bridge is itself in the `StringBuilderLength`
     intrinsic table.

     **Fix**: removed `length` from `is_string_builder_layout_native_override`'s
     blanket-immune list entirely (matching `substring(int)`'s existing,
     never-immune treatment), and added the missing redefine guard to
     `populate_virtual_invoke_cache`'s intrinsic-population block. Verified
     via `InvocationCountProbe` (reflects
     `Mockito.mockingDetails(mock).getInvocations()`) matching real HotSpot's
     exact `length()`/`substring(0)`/`verify()` invocation-count sequence
     byte-for-byte, then both full test classes: 5/5 and 6/6.
     `cargo test -p cratonvm-vm --lib`: 2229 passed, 13 failed (identical
     pre-existing baseline). `intrinsic_diff` differential suite: 4/4
     passed.

     **KNOWN REMAINING GAP (not hit by any currently-passing suite class)**:
     a REAL (non-mock) receiver's `.length()`, called after some OTHER
     StringBuilder has been Mockito-redefined ANYWHERE in the process, now
     falls through the woven advice's "not mocked" branch into
     `AbstractStringBuilder.length()`'s original `getfield count:I` — which
     reads the wrong field index against CratonVM's 2-field (`char[]`,
     `int`) synthetic layout and silently returns `0` instead of the real
     length (confirmed via a dedicated probe, `RealAfterMockLengthProbe`).
     This is the EXACT SAME latent risk `substring(int)` has carried,
     unaddressed, since bug 3 above — not a regression this fix introduces,
     just the same known tradeoff now also applying to `length()`. A real
     fix needs an authoritative per-instance "is this receiver actually
     mocked" signal reachable from Rust WITHOUT re-entering bytecode
     dispatch for the same (class, method) pair — a naive
     `MockUtil.isMock()` + re-invoke-bytecode attempt during this session's
     investigation infinite-looped, since re-invoking "this method's
     bytecode" from inside the very native registered for it re-triggers
     the identical force-native decision.

     Investigation artifacts (Azure host,
     `/data/tmp/mockitobean-substring-20260723/`, none checked in):
     `InvocationCountProbe.java` (reused from the previous session),
     `RealAfterMockLengthProbe.java` (new), plus a ByteBuddy class dump at
     `/tmp/bbdump/` on the Azure host (ephemeral, not preserved) showing the
     actual woven bytecode. Worktree
     `/data/data/wt-mockitobean-realmock-20260723`, branch
     `fix/mockitobean-realmock-signal-20260723`.
- **`test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests`**
  (0/2) — `executeTestsInParallelWithInstancePerMethod` fails an AssertJ
  `MultipleFailuresError` ("Test Event Statistics", 2 failures);
  `rejectTestsInParallelWithInstancePerClassAndRecordApplicationEvents`
  fails a plain `AssertionError`. JUnit parallel-execution × Spring
  TestContext `ApplicationEvents` recording interaction, not investigated.
- **`test.web.servlet.assertj.MockMvcTesterIntegrationTests`** (72/74) —
  `debugUsesSystemOutByDefault`/`debugCanPrintToCustomOutputStream` both
  fail plain `AssertionError`s (`MockMvcTester`'s `.debug()`/`.print()`
  output-stream-capture assertions). Not investigated.
- **`web.servlet.config.MvcNamespaceTests`.`customConversionService`**
  (24/25) and **`web.servlet.config.annotation.ViewResolutionIntegrationTests`
  .`freemarkerWithExplicitDefaultEncodingAndContentType`** (6/7) — single
  plain-`AssertionError` failures each, not investigated.
- **`web.socket.messaging.StompWebSocketIntegrationTests`** (14/16 per the
  2026-07-20 baseline) — NOT re-verified with full detail this session; a
  200s rerun timed out (this test spins up a real embedded Tomcat per test
  method across 16 methods, and the host was under heavy concurrent load
  from several other sessions' builds/test-runs during this rerun attempt).
  No regression expected from anything touched this session, but the exact
  current pass count needs reconfirming with a longer timeout when the host
  is quieter.

**Confirmed unchanged / out of scope, no action taken:**
`core.io.ResourceTests` (66/68, same 2 `remoteResourceExists*` methods the
doc already flagged), `core.retry.RetryPolicyTests` (22/23, doc's own
"deliberate design choice, not worth fixing" stands),
`scheduling.quartz.QuartzSupportTests` (doc's own "environmental,
`spring-context-support` doesn't compile against the shared checkout"
stands), `beans.factory.xml.XmlBeanFactoryTests` (10/95, unchanged, doc
already has detailed root-causing for 2/10 pointing at a `try_build_replace
_override` `super_cid` class-resolution bug upstream of this file, likely
the same loader-identity family documented elsewhere in this repo's
history).

**Host note:** `/data/tmp/cores` (7.6GB of stale 2026-07-17 core dumps) was
cleared at the start of this session to relieve disk pressure (29G free
after, was 21G). The shared `spring-framework-recheck` checkout used for
classpath generation currently has uncommitted local modifications to
`spring-aop`/`spring-context` (`git status` shows deletions matching the
"corrupted `spring-aop/src` tree" symptom documented in the 2026-07-20 AOT
session) — NOT touched or fixed this session (shared resource, another
session may be mid-use); none of the modules this session's target classes
live in (`spring-core`/`spring-context`/`spring-web`/`spring-webflux`/
`spring-webmvc`/`spring-websocket`/`spring-orm`/`spring-context-support`/
`spring-test`) needed rebuilding, so this didn't block anything, but
whoever continues should check `git status` there before trusting a
`spring-aop`/`spring-orm` rebuild.

## Summary

Started from a full 2912-class suite run (dev `213d93ea`) fully triaged
against HotSpot (see history below), which found **177 confirmed genuine
bugs**. Reconfirmed by rerunning exactly those 263 previously-non-passing
classes on a fresh `dev` merge (`8719dca85`, ~3 days / several hundred
commits later), 4 shards, same settings (`suite-run.sh`, `BATCH=10
BATCH_TO=120 ONE_TO=120`, `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`, real JDK 25).

**121 of the 177 are now fixed.** 56 remain open.

| Of the 177 | Count |
|---|--:|
| Now OK (fixed) | 121 |
| Still FAIL | 38 |
| Still/newly TIMEOUT | 16 |
| Now LOADERR (was TIMEOUT) | 2 |
| **Still open** | **56** |

The 86 environmentally-non-OK classes (73 EMPTY + 13 FAIL matching HotSpot,
not CratonVM bugs) were not rerun individually here but the 263-class rerun
included them — EMPTY count held steady at 73, consistent with them still
being environmental.

## Notable clusters (current state, 2026-07-21)

**15 classes fixed 2026-07-21, six independent root causes.** Fixed on
branch `fix/genuine-buglist-56-20260720`, merged to `dev` at `693702a2a`.

1. **`native-collections` Integer-key `HashMap`/`HashSet` fast-path overlay
   (`DenseIntEntries`) iterated in raw dense-index (ascending key) order**
   instead of real JDK's hash-bucket order (`(capacity - 1) & hash(key)`,
   insertion-order tiebreak within a bucket) — broke `keySet()`/`values()`/
   `entrySet()` iteration for any `HashSet<Integer>`/`HashMap<Integer, V>`.
   Fixed `util.CollectionUtilsTests`, `core.annotation
   .NestedRepeatableAnnotationsTests`, `beans.ConcurrentBeanWrapperTests`.
2. **`StringJoiner.add(CharSequence)` only handled a literal `String`**
   via `read_string`, silently turning any other `CharSequence` (e.g. a
   `StringBuilder` — `AbstractSqlParameterSource.toString()` builds each
   entry that way) into the literal text `"null"`. Falls back to invoking
   the real `toString()` now. Fixed `jdbc.core.namedparam
   .BeanPropertySqlParameterSourceTests`/`MapSqlParameterSourceTests`.
3. **`FilterOutputStream.flush()`/`close()` used the generic
   `ctx.invoke_virtual` native-context dispatcher**, which for a
   dynamically-generated subclass receiver (a Mockito ByteBuddy
   `...OutputStream$MockitoMock...` mock) can silently resolve to an
   inherited default method instead of the receiver's own override, so
   Mockito never saw the delegated call. Switched to
   `invoke_virtual_bytecode_only`. Fixed `util.StreamUtilsTests`.
4. **`proxy_invoke_handler`'s `InvocationHandler.invoke()` result was
   never unboxed** for a primitive-returning proxy method. Harmless for a
   single proxy layer, but a proxy wrapping ANOTHER dynamic proxy re-boxed
   the already-boxed wrapper's object reference into a fresh wrapper's raw
   `int` slot, corrupting the value. Fixed via the existing
   `proxy_unbox_primitive_return` helper (previously only used by the
   `AnnotationProxy` branch). Fixed `aop.framework.autoproxy
   .BeanNameAutoProxyCreatorTests` (`proxyWithDoubleProxying`).
5. **Classes with no bytecode of their own** (`cratonvm/internal/*`
   synthetic wrappers, and real JDK types CratonVM represents directly as
   an instance of their own interface's `ClassId`, e.g. `java.lang.reflect
   .TypeVariable`) have exact-class natives that `find_method_recursive`'s
   hierarchy walk can't see, landing on an inherited `java.lang.Object`
   method instead (identity-hash `toString()` instead of e.g. `"[foo,
   bar]"` or a `TypeVariable`'s name). Reflective `Method.invoke()` and any
   native-code-initiated `ctx.invoke_virtual` hit this; ordinary bytecode
   `invokevirtual` doesn't (the interpreter's own dispatch checks the
   native registry first). Check the exact-class native first for this
   namespace/shape before the hierarchy walk. Fixed `expression.spel
   .MethodInvocationTests`/`SpelCompilationCoverageTests`,
   `core.GenericTypeResolverTests`.
6. **`HttpURLConnection.getLastModified()`/`getHeaderFieldDate()`
   unconditionally returned 0/the fallback for any real http(s) URL**,
   ignoring the actual response's `Last-Modified` header entirely. Added a
   minimal RFC 1123 date parser and routed both accessors through the real
   response headers. Also fixed a GC-safety bug in `huc_perform` (the
   header array + per-header `String` + body `byte[]` allocations can
   relocate `this`; needed pin/re-read, both inside `huc_perform` and in
   its three callers) — improves header handling generally but does NOT
   fully fix `core.io.ResourceTests`' `lastModified()` methods (see below).

Also confirmed fixed as side effects (no source change needed, just
re-verified): `jms.core.JmsTemplateTransactedTests`,
`test.context.BootstrapUtilsTests`, `test.context.testng
.TestNGConcurrencyTests`, `scheduling.quartz.QuartzSupportTests` (17
found/9 succeeded/0 failed — the rest skipped, no failures; the
`spring-context-support` module compile gap noted in the 2026-07-20 entry
below is no longer reproducible against the current `spring-framework-
recheck` checkout).

Regression-checked: `cargo test -p cratonvm-vm --lib --release` — 2227
passed / 9 failed post-merge (all 9 the pre-existing `runtime::lock_order`
release-mode-only "should panic" assertions; the `jit::skip_list` env-
gated failures present pre-merge are gone, fixed by unrelated concurrent
work merged from `origin/dev`), no new failures.

**Still open, not fixed by this session:**
- `core.io.ResourceTests`: 2/68 methods (`remoteResourceExists`/
  `remoteResourceExistsFallback`) still return `0` from `lastModified()`
  for this specific `MockWebServer` HEAD-then-GET-fallback scenario,
  despite fix 6 above correctly parsing the header in an isolated
  `HttpURLConnection` probe — root cause not yet found; investigation was
  cut short by host instability (an unplanned reboot, then heavy
  concurrent load from other sessions causing repeated hangs on this
  exact test class).
- `core.retry.RetryPolicyTests`: 1/23 fails on a `toString()` regex
  expecting `"Lambda"` in a composed `Predicate`'s class name; CratonVM
  implements `Predicate.and()`/`or()`/`negate()` as named synthetic classes
  (`Predicate$And`/`$Or`/`$Negate`) rather than synthesizing true lambdas —
  a deliberate, widely-used design choice, not a bug worth touching for
  one cosmetic assertion.
- `jndi.JndiObjectFactoryBeanTests`: 1/25 fails
  (`lookupWithExposeAccessContext` — Mockito verifies `context.close()`
  called 2 times but sees 3, an extra close through an
  `exposeAccessContext` JDK dynamic proxy) — not yet investigated.
- `beans.factory.xml.XmlBeanFactoryTests`: 10/95 still fail (unchanged from
  the 2026-07-20 reconfirmation). Root-caused 2 of the 10
  (`overrideMethodByArgTypeAttribute`/`overrideMethodByArgTypeElement`,
  `<replaced-method>`/`ReplaceOverride` with `<arg-type>` overload
  disambiguation) partway: `native-builtins::spring_startup_bootstrap
  ::try_build_replace_override` mapped `methodName -> replacerBeanName`
  by NAME ONLY, ignoring `<arg-type>` entirely -- fixed by reading each
  `ReplaceOverride`'s real `getTypeIdentifiers()` (Spring 6.2.9+) and
  replicating `ReplaceOverride.matches(Method)`'s exact algorithm
  (overloaded-name arg-substring matching) in
  `jvm_descriptor_param_types_dot_notation`/`jvm_type_to_java_name`. This
  fix is real and landed (more correct than before for the general
  multi-overload-with-different-replacers case), but did NOT close these
  2 tests: traced with `CRATONVM_DBG_REPLOVR` to find the true blocker --
  `try_build_replace_override`'s `super_cid` parameter resolves to the
  WRONG class entirely for these 2 beans (`org/springframework/beans
  /factory/xml/SerializableMethodReplacerCandidate`, an unrelated helper
  class from a different test method in the same file, instead of the
  bean's actual declared class `OverrideOneMethod` -- confirmed via
  `javap` that the real `OverrideOneMethod.class` correctly has all 3
  `replaceMe()`/`replaceMe(int)`/`replaceMe(String)` overloads). This is a
  class-resolution/caching bug upstream of `try_build_replace_override`
  (in whatever resolves a `RootBeanDefinition`'s declared class to a
  `ClassId` before this function is called) -- likely the same family as
  other loader-identity/class-resolution bugs documented elsewhere in
  this repo's history, but not yet traced to its own root cause. The
  other 8/10 `XmlBeanFactoryTests` failures (`rejectsOverrideOfBogusMethodName`,
  `classNotFoundWithDefaultBeanClassLoader`,
  `replaceNonOverloadedInterfaceMethodWithoutSpecifyingExplicitArgTypes`,
  and the 3 CGLIB config-class-adjacent `context.annotation.*` failures
  below) were not investigated this session.

## Notable clusters (2026-07-20 session)

**JMX — 26/26 fixed, cluster fully closed (2026-07-20).** The systemic
`RequiredModelMBean` breakage flagged on 2026-07-17 was resolved for all but
two classes (`jmx.access.MBeanClientInterceptorTests` 11/14,
`jmx.access.RemoteMBeanClientInterceptorTests` 2/14); both are now 14/14.
Two distinct regressions, both introduced after the 2026-07-05
jmx-platform-mxbean-registration fix and neither noticed until this session:
(1) a 2026-07-14 defensive Bridge override
(`register_management_factory_platform_server_stub`, called from the
real-JDK native-registration branch in `vm/src/vm/vm_init.rs`) was left
permanently wired in after the NPE it worked around
(`ObjectName.getCanonicalKeyPropertyListString()` on the synthetic
1-field ObjectName model) was independently fixed elsewhere — it silently
shadowed real `MBeanServerFactory.createMBeanServer()` bytecode with an
empty synthetic `MBeanServer` in real-JDK mode, so `getPlatformMBeanServer()`
registered ZERO platform MXBeans (not even `MBeanServerDelegate`) instead of
the expected ~16. Removed the call, restoring the original KAFKA-MBEAN
design intent (`native-builtins/src/jmx.rs`'s `register_management_factory`
already deliberately leaves this method unregistered for exactly this
reason). (2) `ObjectName.getSerializedNameString()` (real bytecode reached
from `writeObject()`'s non-compat branch) walks the never-populated
`_kp_array` field and NPEs the first time an `ObjectName` is genuinely
Java-serialized — only exercised by the real jmxmp remote
`MBeanServerConnection` wire protocol, not the in-process `MBeanServer`
path the rest of the synthetic ObjectName natives cover. Added a native
override (`RKC-ObjectName-03` in `jmx.rs`) deriving the same canonical text
from the existing text model, matching the established
`getCanonicalKeyPropertyListString` pattern. Full `jmx.*` suite (32 classes,
all `jmx.access`/`jmx.export`/`jmx.support` tests) reconfirmed 100% passing
after the fix, no regressions. Landed on `dev` at `6923fcb9c`
(`fix/jmx-cluster-fix-20260720`).

**AOT/TIMEOUT cluster — 16 classes, still fully hung**, plus 2 that flipped
from TIMEOUT to LOADERR (worth checking — a status-type change, not just
timing): `beans.factory.aot.BeanDefinitionMethodGeneratorTests` and
`beans.factory.aot.InstanceSupplierCodeGeneratorTests`. The still-hanging 16
grew slightly from the original 12 (picked up `test.context.aot.AotIntegrationTests`,
`web.service.registry.HttpServiceProxyRegistrationAotProcessorTests`,
`core.io.buffer.DataBufferTests`, `scripting.groovy.GroovyScriptFactoryTests`
— the last was FAIL before, now TIMEOUT). Full list in the table below
(`beans`, `context`, `orm`, `test`, `web` sections, all `TIMEOUT`/`LOADERR`
rows).

## AOT cluster — 2026-07-20 evening session (JIT miscompilation root-cause + fixes)

**Two genuine, previously-unknown JIT miscompilation bugs found and fixed**
(dev `a8165d607`), plus a fresh from-scratch rebaseline of the whole
15-class AOT cluster against the fix. This is a different root cause
family than the loader-identity bugs fixed 2026-07-15/16 — those were
real and are still in place, but a *separate* defect in the JIT's x64
lowering of two `com.sun.tools.javac` methods was independently
corrupting most of the compile-heavy AOT codegen tests whenever
`TestCompiler` ran enough in-process `javac` invocations in one JVM
(each AOT test method does its own `getTask().call()`; a whole-class run
is 20-50+ such calls in one process).

**Root-caused with a Spring-free, ~40-line standalone repro**
(`ToolProvider.getSystemJavaCompiler().getTask(...).call()` looped
in a plain Java `main`, no Spring/JUnit involved) — reproduces
deterministically at the 20th call every time, isolating this
entirely from Spring/AOT-specific machinery:

1. **`com.sun.tools.javac.jvm.ClassReader.readClass`** — once
   tier-compiled, throws `NullPointerException: Cannot read field "kind"
   because "sym" is null` from inside `Symbol.packge`, reached via
   `ClassReader.readClass -> readClassBuffer -> readClassFile ->
   ClassFinder.fillIn -> Modules$1.complete` (module-graph symbol
   completion during `Modules.setupAllModules`). Confirmed JIT-only
   (`--nojit` / `CRATONVM_JIT_THRESHOLD=100000` both prevent it) and
   bisected to this exact method via `CRATONVM_JIT_BISECT_SKIP=
   com/sun/tools/javac/jvm/ClassReader.readClass`. Fixed by adding it to
   the JIT interpreter-fallback skip-list (`vm/src/jit/skip_list.rs`,
   `SkipReason::ClassReaderReadClass`).
2. **`com.sun.tools.javac.code.ClassFinder.complete`** — a second,
   distinct residual in the same scenario, surfacing even with (1)
   fixed. Two symptoms: a `-Werror`/`@SuppressWarnings("deprecation")`
   false positive (the suppression annotation IS present in the
   generated source but real javac's `-Werror` still fails the
   compile), and outright duplicated tokens in generated source (e.g.
   `import import org.springframework.aot.generate.Generated;`).
   Bisected the same way (`CRATONVM_JIT_DENY=
   com/sun/tools/javac/code/ClassFinder` then `CRATONVM_JIT_BISECT_SKIP=
   .../ClassFinder.complete`). Fixed via
   `SkipReason::ClassFinderComplete`.

Both fixes are narrowly scoped (single named method each), regression-
checked (`cargo test -p cratonvm-vm --lib --release`: 2218 passed / 17
failed, byte-identical to the documented pre-existing lock_order/
skip_list release-mode baseline both before and after), and merged to
`dev` (`a8165d607`).

**Rebaseline after the fix** (fresh worktree, from-scratch
`spring-framework-recheck` checkout — see host-state note below — real
JDK 25, per-class timeouts raised to 300-550s since interpreter-fallback
adds real overhead to these compile-heavy tests):

| Class | Before | After | Notes |
|---|---|---|---:|
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | TIMEOUT | **OK 14/14** | fixed |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | TIMEOUT/LOADERR | **OK 47/47** | fixed (needs ~470s, not 120-350s) |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | TIMEOUT | **OK 44/44** | fixed (needs ~460s) |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | LOADERR | **OK 26 found/24 succ/0 fail** (2 skip) | fixed |
| `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` | TIMEOUT (FAIL 8/2/6 historically) | **OK 8/8** | fixed |
| `test.context.aot.TestClassScannerTests` | TIMEOUT | **OK 7/7** | fixed |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT | TIMEOUT (perf partially fixed — see below) | **genuine, severe performance defect — do not call this "not a bug".** Originally ~54 minutes (`3242228ms`). Measured HotSpot on the SAME classpath/JDK: **`13126ms` (13.1s)**, ~247x slower. **2026-07-21 session: root-caused and fixed the dominant lever.** gdb sampling of the largest method (`applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles`, 10001 bean definitions) found `Arena::free_list_bytes()`'s summation closure dominating 3/5 stack samples — its epoch-gated cache (2026-07-15) degrades back to O(free-list-size) per call under steady allocation churn (content changes on nearly every call from its only caller, `needs_gc`, which runs on every allocation), and the list never fully drains over a session. Replaced with an incrementally-maintained running total (`gc/src/arena.rs`), making it unconditionally O(1); also memoized `force_native_over_real_jdk_bytecode` for uncached dispatch paths (reflective `Method.invoke()`, megamorphic call sites) reached via Mockito's constructor-mock dispatch. Merged `49b75fa20`. Measured impact: the fixed method alone dropped 379s -> 179s (2.13x); full 14-method class dropped 3242s -> 2976s (~8.2% aggregate -- the other 13 methods don't hit the same free-list-growth pathology as severely, since it scales with allocation volume and only that one method allocates ~10k objects). Residual ~227x-vs-HotSpot gap remains and needs further investigation beyond the free-list fix -- the next lead is whatever dominates the OTHER 13 methods' time, not yet profiled. |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | LOADERR | **OK 34/34 (2026-07-21, FIXED)** | **fully fixed.** The 3 residuals (2 as of a 2026-07-21 rebaseline — `generateBeanDefinitionMethodWhenInnerBeanGeneratesMethod` content-corruption + `generateBeanDefinitionMethodUSeBeanClassNameIfNotReachable`'s `ClassCastException: String cannot be cast to TypeName`) were a FOURTH javac-adjacent JIT residual, this time in Spring's own shaded JavaPoet, not javac itself: `org/springframework/javapoet/CodeBlock$Builder.add(String, Object...)` (the `$`-placeholder format-string parser). Bisected with `CRATONVM_JIT_DENY`/`CRATONVM_JIT_BISECT_SKIP` the same way as fixes (1)/(2): denying the whole `CodeBlock` class does nothing, denying `CodeBlock$Builder` fixes both symptoms, and narrowing further rules out `argToType`/`addArgument` individually — only `add` itself (which inlines `argToType`'s instanceof-guarded `checkcast` into its own compiled body) is sufficient. Added `SkipReason::JavaPoetCodeBlockBuilderAdd` to `vm/src/jit/skip_list.rs`. Verified 34/34 OK, deterministic across 3 repeat runs. `cargo test -p cratonvm-vm --lib --release`: 2227 passed / 9 failed, same pre-existing release-mode lock_order baseline before/after. Landed on `fix/aot-cluster-residuals-20260721` (`20abd63d0`), not yet merged to `dev`. |
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT | FAIL **40/32/8** (2026-07-21 session; was 40/16/24) | **2026-07-21: found and fixed 6 compounding bugs in the native ConfigurationClassEnhancer CGLIB-proxy reimplementation** (`native-builtins/src/cglib_enhancer.rs`), all surfaced by the single dominant family "any @Configuration class using constructor injection": (1) generated proxy constructor was always no-arg regardless of the superclass's real constructor -- fixed by emitting one delegating constructor per non-private superclass constructor; (2) generated class was named with cglib's default `$$EnhancerByCGLIB$$` tag instead of Spring's own `SpringNamingPolicy` `$$SpringCGLIB$$` tag, so even a correctly-built class was invisible under the name generated source references; (3) the native reimplementation never notified `ReflectUtils.generatedClassHandler`, so Spring AOT's `GeneratedFiles` capture (needed for the LATER compile step to resolve the proxy class) never fired; (4) the per-superclass class-identity cache (added earlier, load-bearing for a different test) skipped that notification entirely on a cache hit, so a SECOND test enhancing an already-cached class never got its own `GeneratedFiles` populated; (5) real CGLIB emits `CGLIB$SET_STATIC_CALLBACKS`/`CGLIB$SET_THREAD_CALLBACKS` stub methods on every generated class that `Enhancer.isEnhanced()`/`registerStaticCallbacks()` reflectively check for -- added as no-op stubs since this reimplementation never uses a real callback array; (6) resolving `ReflectUtils` by a loader-agnostic (or enhanced-class-scoped) lookup could resolve the WRONG `ReflectUtils` instance under `@CompileWithForkedClassLoader` (each test gets its own forked child loader for infrastructure classes) -- fixed by resolving via the `enhance()` call's own receiver's loader instead. Merged `da109dc5a`. Verified 16/40 -> 32/40 (31/40 on a from-scratch merge-tip rebuild, small variance consistent with this class's already-documented cross-test-timing sensitivity). Remaining 8 residuals include at least one distinct, unrelated bug: `@Value`-annotated field injection not reaching the proxied instance (`processAheadOfTimeWhenHasCglibProxyAndMixedAutowiring` now compiles and runs but asserts `"Hi null"` instead of `"Hi AOT World"`) -- not yet root-caused, a separate area from proxy generation itself. |
| `test.context.aot.AotIntegrationTests` | TIMEOUT | FAIL **4/0/2/2** (2026-07-21 rebaseline: found=4 succ=0 fail=2 skip=2) | **new dominant failure as of 2026-07-21** (supersedes the `TestContextAotException` shape below -- that may still be the residual once this is fixed, not yet re-checked): `java.lang.IllegalStateException: A custom 'searchEnclosingClass' predicate can only be combined with SearchStrategy.TYPE_HIERARCHY`, thrown from `MergedAnnotations$Search.withEnclosingClasses` via `TestContextAnnotationUtils.hasAnnotation` <- `TestContextAotGenerator`'s `isDisabledInAotMode` predicate. The calling code literally does `MergedAnnotations.search(SearchStrategy.TYPE_HIERARCHY).withEnclosingClasses(...)` in one expression -- the guard should trivially pass. Confirmed via a standalone minimal repro (`SearchStrategyProbe.java`, same call shape, no Spring-test/AOT machinery) that this is **not** a general enum `==` bug: the isolated repro passes cleanly on the SAME binary. The failure is specific to the real AOT/`@CompileWithForkedClassLoader` context. `CompileWithForkedClassLoaderClassLoader`'s constructor deliberately sets its OWN parent to `testClassLoader.getParent()` (skipping `testClassLoader` itself) and its `findClass` redefines any class it can pull bytes for via `testClassLoader.getResourceAsStream(...)` -- so framework classes (`MergedAnnotations`, `SearchStrategy`, `TestContextAnnotationUtils`) get a genuinely FRESH `Class`/enum-constant identity per forked test, by design (matches real CGLIB/Spring behavior, works fine on HotSpot). Suspected root cause: some CratonVM-side cache/registry (class definition, enum constant, or similar) is keyed by NAME ONLY rather than by (name, loader), letting one of the two sides of the `==` comparison resolve to a STALE instance from an earlier forked-loader instance instead of the current one -- the exact same bug *shape* as the StackWalker regression and the ApplicationContextAotGeneratorTests ReflectUtils-notification bug fixed the same session, just not yet localized to a specific cache/table. Ruled out: a standalone repro mimicking `CompileWithForkedClassLoaderClassLoader`'s EXACT parent-skip + resource-byte-redefine behavior (`ForkedLoaderProbe.java`/`SearchStrategyWorker.java`, no JUnit Platform involved), running the identical `MergedAnnotations.search(...).withEnclosingClasses(...)` call 3x through 3 fresh forked-loader instances, passed cleanly every time -- so the classloader-fork mechanism ALONE isn't sufficient to trigger it; the real cause needs something else specific to the full JUnit Platform Launcher machinery and/or `TestContextAnnotationUtils`/`TestContextAotGenerator` themselves (their own static state, or a DIFFERENT/nested classloader boundary somewhere in that path). Next step: instrument `identityHashCode`/`getClassLoader()` at both operands of the `==` directly inside a copy of `TestContextAnnotationUtils.hasAnnotation`, or bisect by replacing pieces of the JUnit Platform launch path in the standalone repro until it starts reproducing. |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL 0/4 | FAIL **4/0/4** (2026-07-21 rebaseline: found=4 succ=0 fail=4, regressed from 2/4 passing) | **same new dominant failure as AotIntegrationTests above** (`IllegalStateException: A custom searchEnclosingClass predicate...`) now hits ALL 4 methods (`processAheadOfTimeWithWebTests`, `processAheadOfTimeWithBasicTests`, `endToEndTests`), except `processAheadOfTimeWithXmlTests` which still shows the older `TestContextAotException: Failed to process test class [...XmlSpringVintageTests] for AOT` shape -- fix the searchEnclosingClass bug first, then re-baseline this class's residuals (the previously-documented 2 residuals below may or may not still apply). |
| `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` | TIMEOUT | FAIL **5/3/2** | now completes; **NEW finding, NOT JIT-related** (confirmed via `--nojit`, identical 3/5 either way): `java.lang.ArrayStoreException: arraycopy: source element at index 0 is not assignable to destination component type` inside `tools.jackson.databind.util.ArrayBuilders.insertInListNoDup`, thrown while creating the `httpServiceProxyRegistry` bean. Not yet root-caused — likely a reflection/generic-array-creation type bug feeding Jackson a wrongly-typed array, upstream of the `arraycopy` covariance check (which is behaving correctly by rejecting it). |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL 3/5 | FAIL **3/5** (unchanged count, but the ORIGINAL `ClassCastException: Class cannot be cast to String[]` this doc documented as fixed 2026-07-16 is confirmed gone) | same `ArrayStoreException` as the sibling class above — shared root cause, 2 methods (`basicListingWithAot`, `basicScanWithAot`). The previously-documented JDK24+ `java.lang.classfile.ClassFile` host gap does NOT explain this (host now runs real JDK 25 throughout this session's testing, which has that API). |
| `context.annotation.ConfigurationClassPostConstructAndAutowiringTests` | FAIL 1/2 | FAIL 1/2 (unchanged) | **out of scope** — verified this is a `@PostConstruct`/`@Autowired` circular-init bean-lifecycle bug (`UnsatisfiedDependencyException` on `setTestBean`), nothing to do with AOT code generation. Likely miscategorized into this doc's AOT-cluster table originally; leave for a bean-lifecycle investigation, not this cluster. |
| `context.annotation.ConfigurationClassPostProcessorTests` | FAIL 82/85 | FAIL 82/85 (unchanged) | **out of scope** — verified failures are CGLIB proxy method lookup (`NoSuchMethodException: getTestBean`), `@Bean` null-argument handling, config-override validation — none touch AOT/TestCompiler. Do not confuse with the separately-named, already-fixed `ConfigurationClassPostProcessorAotContributionTests` (see the 2026-07-15/16 loader-identity docs) — this is a different class. |

**Host-state gotchas hit and fixed this session** (worth knowing for
whoever continues): the shared `spring-framework-recheck` checkout used
for classpath generation had (a) a corrupted `spring-aop/src` tree
(283 of 314 `.java` files — the `aop.target` package was entirely
missing, breaking `spring-orm` compilation) and (b) a `spring-beans`
jar with one mismatched class-file entry (`AbstractBeanDefinition.class`
containing `BeanDefinition`'s bytecode) plus several modules' test-
fixtures jars (`*-test-fixtures.jar`) simply absent from `build/libs`.
Both were fixed by restoring `spring-aop/src` from the known-good
Windows reference checkout and force-rebuilding the affected jars
(`--rerun-tasks`). Neither was a CratonVM bug — both were host/checkout
corruption (plausibly from the same disk-pressure-driven "harvester"
process documented elsewhere in this repo's known-issues history) — but
they were initially indistinguishable from real compile failures and
cost real investigation time before being ruled out. **Always verify
the classpath/checkout integrity first** when a whole cluster of
AOT/compile-based tests shows the exact same `CompilationException`
shape.

**Recommended next steps for whoever continues this cluster (updated
2026-07-21 late session — see that section below for the full detail
behind each item):**
1. ~~Bisect `BeanDefinitionMethodGeneratorTests`'s 3rd JIT residual~~ —
   DONE, class is 34/34 OK.
2. ~~Re-verify `ApplicationContextAotGeneratorTests` with `--nojit`~~ —
   DONE: confirmed only partially the same JIT family (32/40 JIT-enabled
   after the JavaPoet fix vs. 33/40 under `--nojit` with none of the JIT
   fixes needed) — a 7th CGLIB counter-scoping bug (now fixed, see below)
   accounts for most of the rest.
3. The `TestContextAotException` next step is SUPERSEDED — the dominant
   failure in both `test.context.aot.*` classes is now the
   `searchEnclosingClass`/`SearchStrategy` duplicate-`ClassId` bug (see
   below); `KRUN_STACK=1` is no longer the useful lever,
   `CRATONVM_DBG_DUPCLASS=1` is.
4. The `ArrayStoreException` finding IS root-caused now (see below) but
   NOT fixed — it's the SAME duplicate-`ClassId` mechanism as item 3, one
   level removed (an interface, not an enum). Fix both together.
5. **NEW**: the duplicate-`ClassId`-under-`@CompileWithForkedClassLoader`
   mechanism itself (items 3+4's shared root cause) needs a dedicated,
   careful session — likely the single highest-leverage remaining AOT-
   cluster fix, since it plausibly also explains some of
   `ApplicationContextAotGeneratorTests`'s remaining residuals
   (`processAheadOfTimeUsesCglibClassForFactoryMethod`'s intermittent
   `"is not an enhanced class"`) and possibly other `@CompileWith
   ForkedClassLoader`-using classes elsewhere in the suite not yet
   connected to this finding. See the recommended-next-step paragraph in
   the 2026-07-21 late session section for the two candidate fix shapes.
6. `beans.factory.aot.BeanRegistrationsAotContributionTests` perf: the
   free-list O(1) fix landed but the class is still ~227x slower than
   HotSpot and TIMEOUTs; profile which of the OTHER 13 methods (only 1 of
   14 hit the free-list pathology) dominates next.

## AOT cluster — 2026-07-21 late session (4th JIT bug, CGLIB counter fix, duplicate-ClassId unifying finding)

**`beans.factory.aot.BeanDefinitionMethodGeneratorTests` — FIXED, 34/34.**
See the updated table row above for the full writeup: a fourth JIT
miscompilation, this time in Spring's shaded JavaPoet
(`org/springframework/javapoet/CodeBlock$Builder.add`) rather than javac
itself. `SkipReason::JavaPoetCodeBlockBuilderAdd` added to
`vm/src/jit/skip_list.rs`.

**`context.aot.ApplicationContextAotGeneratorTests` — found and fixed a
7th CGLIB-proxy bug, on top of the 6 already landed as `da109dc5a`
earlier the same day.** The `$$SpringCGLIB$$<n>` proxy-name counter
(`native-builtins/src/cglib_enhancer.rs::next_config_enhancer_counter`)
was keyed by superclass name ALONE. That's correct for the single-method
case the counter was originally added for
(`AnnotationConfigApplicationContextTests.refreshForAotRegisterHintsForCglibProxy`,
one enhancement of `CglibConfiguration` per JVM), but
`ApplicationContextAotGeneratorTests` has SEVERAL `@Test` methods that each
enhance their OWN fixture class sharing the simple name `CglibConfiguration`
— `@CompileWithForkedClassLoader` gives each test method a fresh child
loader, so these are genuinely distinct `ClassId`s, not repeat enhancements
of one class — and since `KRun` batches every `@Test` method of a class into
one JVM process, the second and third such methods inherited the first
one's already-incremented counter and got suffix `1`/`2` instead of the `0`
every one of them independently expects (real CGLIB's own
`AbstractClassGenerator` naming/cache state lives in a per-`ClassLoader`
map, so a fresh loader always restarts the count on HotSpot). Rekeyed the
counter by `(defining_loader_id, super_internal_name)` instead of the name
alone — `native_array_new_array` also now prefers the component mirror's
own `ClassId` over re-resolving by name, matching the existing
`class_id_defined_by_loader_exact` pattern used a few lines away in
`getComponentType()`.

Isolating the two fixes' individual contributions (both on top of the
already-landed 6-bug CGLIB session and both AFTER a from-scratch rebuild):
JavaPoet fix alone (JIT enabled, no counter fix) measured **32/40**, all 8
residuals CGLIB/autowiring-shaped; `--nojit` (JIT effectively off, no JIT
fixes needed at all) independently measured **33/40**, confirming most but
not all of the residual set is JIT-independent. This class is **extremely**
sensitive to host contention — repeat clean-room runs later the same
session intermittently OOM'd/LOADERR'd purely from unrelated concurrent
sessions' heavy JVMs on the shared Azure host (an ES perf test at `-Xmx
4g`, a WildFly surefire run), not from these fixes; treat any single run's
exact pass count on this class as noisy and prefer a multi-run median, per
the class's own already-documented cross-test-timing sensitivity. Remaining
residuals include the previously-documented `@Value`-field-injection gap
(`processAheadOfTimeWhenHasCglibProxyAndMixedAutowiring`) and
`processAheadOfTimeUsesCglibClassForFactoryMethod`'s
`IllegalArgumentException: ... is not an enhanced class` (order-dependent,
only seen on some runs — likely the SAME underlying duplicate-`ClassId`
mechanism below, not yet confirmed).

**Unifying root-cause finding (NOT fixed, needs a dedicated session):
`searchEnclosingClass` (`test.context.aot.*`) and the Jackson
`ArrayStoreException` (`web.service.registry.*`) are the SAME bug shape.**
Added an opt-in diagnostic (`CRATONVM_DBG_DUPCLASS=1`,
`classloading/src/class_manager.rs::resolve_fast_path_class_id`) that logs
whenever a name lookup REJECTS an existing `UserDefined`-loader class
registration in favor of creating a fresh one under `Application` (because
the built-in delegation chain can also find the class's bytes — see that
function's own doc comment for why this is deliberate, added to fix an
earlier, different bug). Running `AotIntegrationTests` with it on shows
`org/springframework/core/annotation/MergedAnnotations$SearchStrategy`
(the exact enum `withEnclosingClasses`'s `IllegalStateException` compares
with `==`) hitting this rejection path 3 times — i.e. the SAME class name
is registered under (at least) two DIFFERENT `ClassId`s: one under the
`@CompileWithForkedClassLoader` fork's own `UserDefined` loader (which
redefines every framework class it can pull bytes for, by design, so code
running inside that forked context should see ITS OWN `SearchStrategy`
identity) and a second, separate one created by any loader-BLIND
name-only resolution helper (`ensure_class_initialized`/
`load_class_concurrent`/`resolve_fast_path_class_id`) reached from that
same forked context — those helpers have no notion of "which loader is
asking" and default to preferring `Application` whenever the built-in
chain can also serve the class, silently creating a duplicate instead of
reusing the forked loader's copy. `MergedAnnotations.search(SearchStrategy
.TYPE_HIERARCHY).withEnclosingClasses(...)`'s two `SearchStrategy.
TYPE_HIERARCHY` references (the literal passed to `.search(...)` and the
one `withEnclosingClasses` compares against with `==`) can therefore
resolve to two numerically-different-but-logically-identical enum
constants depending on which resolution path each one took.

Traced the EXACT SAME mechanism independently for the
`web.service.registry.*` `ArrayStoreException`: `tools/jackson/databind/
deser/KeyDeserializers` (an interface, not an enum, but the identical
duplicate-`ClassId`-for-one-name shape) is registered under two different
`ClassId`s; `old.getClass().getComponentType()` re-derives the component
by NAME (`ensure_class_initialized`) rather than reading back the array's
own already-correct component `ClassId`, so `Array.newInstance(...)`
allocates a new array tagged with the WRONG (duplicate) `ClassId`, and the
subsequent `System.arraycopy` of the old elements into it correctly
throws `ArrayStoreException` against that mismatch. Tried the
locally-obvious fix (route `getComponentType()`'s array branch through the
class registry's own `array_info.component_class_id` instead of the name
string) but discovered `array_info` is populated `None` at EVERY
class-construction site in the codebase (`grep -rn 'array_info: Some'`
across `classloading/src/` returns zero matches) — it's a fully unwired
stub, not a locally-fixable gap, so that patch was reverted rather than
landed as dead code.

**Recommended next step for whoever continues this specific finding**:
this is a genuine, structural gap — native helpers that resolve a class
by NAME ALONE (`ensure_class_initialized`, and whatever underlies
`Array.newInstance`'s reflective component resolution) have no way to
know which loader/context is asking, so under
`@CompileWithForkedClassLoader` (and likely any other scenario where a
custom loader redefines a framework class already reachable via the
built-in delegation chain) they can silently duplicate a class the
CALLER's own context already has a perfectly good copy of. A full fix
needs either (a) threading the CALLER's defining-loader id through these
name-only resolution helpers so they can consult
`class_id_defined_by_loader_exact` first (the pattern already used
correctly a few lines away in `native_class_get_component_type`'s
`classLoader`-field branch), or (b) wiring up `array_info` properly at
every array-class synthesis site so `array_component_class_id` (already
present on the `NativeContext` trait for exactly this purpose) stops
being a permanent no-op. Both are bigger, riskier changes than fit safely
in one sitting — reproduce first with `CRATONVM_DBG_DUPCLASS=1` on
`AotIntegrationTests` or the `web.service.registry.*` classes before
attempting either.

**Follow-up same session: landed a real, partial improvement, but the
full fix is bigger than initially scoped -- three distinct loader-blind
code paths identified, not one.** Added `CRATONVM_DBG_DUPCLASS_BT=1`
(full backtrace on every rejected duplicate-registration) to make this
tractable, then traced both bugs to their EXACT call sites:

1. **`searchEnclosingClass` goes through `resolve_class_loader_aware` /
   `should_use_loader_initiated_resolution`** (`vm/src/runtime/
   interpreter.rs`) -- the SAME loader-aware `CONSTANT_Class`/field-ref
   resolution mechanism already built (and gated off by default) for the
   Tomcat/Hibernate/WildFly custom-loader work, with an EXISTING narrow,
   type-checked carve-out for `GroovyClassLoader`. Widened that carve-out
   to also match Spring's `CompileWithForkedClassLoaderClassLoader`
   (`is_compile_with_forked_class_loader`, mirroring `is_groovy_class_loader`
   exactly -- exact-`ClassId` match, no `is_subclass_of` walk needed since
   the class is `final`). Verified via `CRATONVM_DBG_LOADER_TRACE=1` that
   this DOES work as intended for at least one call site: a `getstatic
   SearchStrategy.TYPE_HIERARCHY` reached from `BootstrapUtils` (itself
   loaded by the fork) now correctly drives the fork's OWN loader first
   and lands on the fork's own consistent `SearchStrategy` `ClassId`,
   instead of falling straight to the global fast path.
   **But this alone does not fix either failing test.** The SAME trace
   shows a SECOND `getstatic SearchStrategy.TYPE_HIERARCHY` -- reached
   from `MergedAnnotations$Search.withEnclosingClasses`'s OWN bytecode
   (the `Assert.state(this.searchStrategy == SearchStrategy.TYPE_HIERARCHY,
   ...)` check that actually throws) -- with `referencing_loader=
   Some(Application)`, NOT the fork. So `MergedAnnotations$Search` itself
   is NOT being given its own forked-loader copy in this VM, even though
   (per Spring's documented design intent, and the doc's own earlier
   writeup) it should be, alongside every other framework class the fork
   redefines. Two references to the same enum constant, resolved via two
   different loaders (fork vs. Application), is the actual mismatch --
   narrower and different from the original hypothesis ("one resolution
   helper is loader-blind"). WHY `MergedAnnotations$Search` itself ends up
   Application-scoped instead of fork-scoped is not yet root-caused --
   likely something in how/when that specific class first got loaded
   in this JVM (possibly before the current test's fork instance even
   existed), which is a `ClassLoader.loadClass()`-level delegation
   question, not a `resolve_class_loader_aware` question -- needs its own
   trace (`CRATONVM_DBG_LOADER_TRACE` widened to also fire on
   `MergedAnnotations$Search`'s OWN class resolution, not just
   `SearchStrategy`'s).

2. **The Jackson `ArrayStoreException` goes through a COMPLETELY
   DIFFERENT path**: `native_object_get_class` (`Object.getClass()`,
   `native-builtins/src/lib.rs`) calls `ctx.load_class(&array_class_name)`
   directly on a synthesized `"[L...;"` descriptor string, which recurses
   into `classloading::class_manager::synthesize_array_class`, which
   resolves the COMPONENT via a bare `self.load_class(component_name)` --
   never touching `resolve_class_loader_aware` at all. So fix #1 above is
   structurally irrelevant to this bug; it needs its own fix in
   `synthesize_array_class` (or its caller). Tried the obvious one --
   populate the always-`None` `array_info` field on the synthesized array
   `Class` with the component `ClassId` this function ALREADY resolves
   internally (pure-additive: nothing currently reads `array_info`, so
   this cannot regress anything; the field's own doc comment even says
   "Wire up real `ArrayInfo` once a consumer... actually reads it", i.e.
   this was always the planned next step) -- but on reflection this does
   NOT reliably fix the bug either: `synthesize_array_class` caches ONE
   array `Class` GLOBALLY per descriptor NAME (not per (loader, name)),
   so whichever caller happens to synthesize `"[Ltools/jackson/databind/
   deser/KeyDeserializers;"` FIRST in the JVM session permanently decides
   `array_info.component_class_id` for every LATER `getClass()` call on
   ANY `KeyDeserializers[]` array, regardless of that specific array's
   own actual (and possibly different) component `ClassId` -- the same
   class-of-bug one level up, just baked into the array-class cache
   instead of the plain-class cache.
   **Deliberately did NOT change the array class's own `loader_id`
   scoping to fix this** (the more "correct-per-JVMS-5.3.3" fix for
   reference-component arrays) -- `synthesize_array_class` has an
   existing, deliberate, audited invariant enforcing `loader_id ==
   Bootstrap` unconditionally regardless of component loader
   ("Round 7 audit fix (CRIT #2)", with a `debug_assert_eq!` guarding
   it and an explicit comment warning future contributors not to change
   `Class::loader_id` without updating the map key too). That invariant
   was presumably added to fix a DIFFERENT, real bug this session has no
   visibility into -- touching it without understanding that history first
   is exactly the kind of change that looks locally correct and
   regresses something else. Left `array_info` un-populated (reverted)
   rather than land a fix that looks plausible but is not verified
   correct.

**Revised recommended next steps**, in order of leverage:
1. Trace `MergedAnnotations$Search`'s OWN class resolution (not
   `SearchStrategy`'s) with `CRATONVM_DBG_LOADER_TRACE`/
   `CRATONVM_DBG_DUPCLASS_BT` to find why it ends up Application-scoped
   instead of fork-scoped inside a `@CompileWithForkedClassLoader` test --
   this is probably a `ClassLoader.loadClass()` top-level delegation bug
   (is `findLoadedClass`/`cl_find_loaded_class` genuinely being consulted
   for EVERY class the fork's `loadClass()` bytecode touches, or is there
   a shortcut somewhere that returns an already-cached Application answer
   without ever asking the fork loader instance at all?), not a
   constant-pool-resolution bug -- different mechanism, different fix
   location, from item 1 above.
2. Before touching `synthesize_array_class`'s loader-scoping, read the
   Round 7 CRIT #2 audit history (git blame / commit message on the
   `debug_assert_eq!` near the end of that function) to understand what
   it was protecting against, so a loader-scoped-for-reference-arrays fix
   can coexist with whatever that was.
3. Once (1) is understood, re-attempt the `array_info` wiring from a
   position of already knowing whether array classes need per-loader
   caching too, rather than guessing.

**Pushed item 1 (above) one step further with `CRATONVM_DBG_LOADER_TRACE`
widened to `MergedAnnotations`/`MergedAnnotations$Search` too, not just
`SearchStrategy`.** At least THREE distinct `MergedAnnotations` outer-class
copies coexist in the SAME JVM run of `AotIntegrationTests` alone: one
under `UserDefined(3)` (one test method's fork), one under `UserDefined(4)`
(a DIFFERENT test method's fork), and one under plain `Application`
(loaded before any fork existed, plausibly by JUnit's own internal
annotation scanning). Each resolves its OWN nested `$Search` class
correctly and self-consistently through the SAME loader
(`UserDefined(3)`'s `MergedAnnotations` -> `UserDefined(3)`'s `Search`;
`Application`'s `MergedAnnotations` -> `Application`'s `Search` --
`resolve_class_loader_aware`/the new carve-out from item 1 works
correctly for ALL three, individually). The ACTUAL failing
`withEnclosingClasses` call executes on an INSTANCE of the
**`Application`-scoped** `Search` class -- meaning whatever code calls
`MergedAnnotations.search(SearchStrategy.TYPE_HIERARCHY)` in the failing
path (`TestContextAnnotationUtils`/`TestContextAotGenerator`'s
`isDisabledInAotMode` predicate, reached via reflection --
`native_method_invoke`/`native_method_invoke_boxed` frames present in
the full backtrace) itself resolves `MergedAnnotations` to the
`Application` copy, not a forked one. If EVERYTHING downstream of that
call also consistently resolved via `Application` (which the "self-
consistent" pattern above says it should), there would be no bug -- so
the actual `SearchStrategy.TYPE_HIERARCHY` value flowing into
`this.searchStrategy` must be getting resolved through a DIFFERENT
loader context than the `Search` instance's own class does. The two
most likely explanations, neither confirmed: (a) the calling method is
itself a lambda/method-reference whose generated class's defining loader
differs subtly from the class that lexically declared it, or (b) the
reflective `Method.invoke()` path (visible in the backtrace) resolves a
literal constant argument in the CALLER frame's context rather than the
declared method's, which is a JIT-adjacent misattribution just like
several of the OTHER argument-decode bugs already fixed elsewhere in
this codebase (see `wildfly-jit-arg-decode-unboxed-primitive-triple-
misattribution` in the fixed-bug archive for the general shape). This
needs live-debugging or per-frame identity instrumentation right at the
`Method.invoke()` boundary to pin down further -- log-based tracing alone
cannot distinguish these two theories. Stopping here for this session;
the `CRATONVM_DBG_LOADER_TRACE` substring widening (`MergedAnnotations`)
is left in place alongside the earlier `SearchStrategy` one for whoever
picks this back up.



**`test.context.jdbc.*` cluster — fully fixed (0 remain).** All 25 classes
that were uniformly failing behind Spring's `ApplicationContext` failure
threshold circuit-breaker now pass. Whatever landed in the last 3 days
resolved the whole cluster at once — worth checking dev history for the
specific fix if attribution matters.

**HTTP JSON/message-converter cluster — fixed 2026-07-20 (8/8 classes).**
`http.converter.json.*` (Gson, Jackson2, MappingJackson2, Jsonb,
Kotlin-serialization), `http.converter.StringHttpMessageConverterTests`,
`http.ContentDispositionTests`, `http.client.SimpleClientHttpRequestFactoryTests`
all now pass 100%. Two independent root causes, both in `native-io`/
`native-builtins`/`vm`:
1. **Shared charset/encoding gap (7/8 classes).** `ByteArrayOutputStream
   .toString(Charset)`/`toString(String)` (`native-io/src/lib.rs`) ignored
   the charset argument entirely and always did lossy UTF-8 decoding —
   fine for ASCII/UTF-8 content, silently mangling anything else (UTF-16BE
   JSON bodies in the `writeUTF16`/`writeObjectInUtf16` tests, ISO-8859-1
   in `StringHttpMessageConverterTests.writeDefaultCharset`, Shift_JIS in
   `ContentDispositionTests.parseQuotedPrintableShiftJISFilename`'s
   RFC 2047 decode, all of which route through this exact JDK method via
   `StreamUtils.copyToString(ByteArrayOutputStream, Charset)`). Fixed by
   routing through the real `cratonvm_native_api::charset` engine using the
   requested charset.
2. **`SimpleClientHttpRequestFactoryTests` (1/8 classes, 3 residual method
   failures after fix 1).**
   - `deleteWithoutBodyDoesNotRaiseException`/`httpMethods`: the synthetic
     `HttpURLConnection.<init>(URL)` native (`native-builtins/src/
     http_url_connection.rs::huc_init`) unconditionally clobbered field 0
     (the real inherited `URLConnection.url`) whenever real JDK code called
     `super(url)` directly on a subclass (not just via `URL.openConnection
     ()`), breaking `getURL()` and real-carrier detection; separately,
     `setRequestMethod` accepted `"PATCH"` (real JDK's whitelist doesn't,
     throwing `ProtocolException` — added as a new `RuntimeError` variant).
   - `interceptor`: a genuinely deep, cross-cutting bug — `Mockito.mock
     (HttpURLConnection.class)` (default "inline" mock maker) redefines the
     class's bytecode IN PLACE via JVMTI rather than subclassing it, so
     CratonVM's redefine-generation counter for `java/net/HttpURLConnection`
     trips permanently for the rest of the process, for EVERY instance —
     including totally unrelated, genuinely real connections created by
     *later* tests in the same JVM. The interpreter's redefine-guard then
     ceded to the (Mockito-woven) bytecode for those real connections too,
     so `getResponseCode()`/`getHeaderField()`/etc. silently no-op'd instead
     of touching the real request/response. Fixed with a receiver-aware
     exemption in `vm/src/runtime/interpreter.rs::intercept_force_registered
     _native`: force the native for `java/net/HttpURLConnection` whenever
     the receiver's field 0 is non-null (a real carrier's populated `url`
     field vs. a Mockito mock's always-null Objenesis-constructed field),
     re-validated per-call so genuine mocks (field 0 stays null) are
     unaffected and still correctly route through Mockito's advice.

Verified via an 8-class targeted run (all 100%) plus a 27-class regression
sweep across `http.client.*`/`web.client.*`/the sibling `http.converter`
cluster (`FormHttpMessageConverterTests`, `BufferedImageHttpMessageConverterTests`,
`Jaxb2CollectionHttpMessageConverterTests`) — no regressions;
`web.client.RestClientIntegrationTests`/`RestTemplateIntegrationTests`
(both pre-existing, out-of-scope failures) even improved (4->2 and 7->3
failing methods respectively), consistent with sharing the same
HttpURLConnection root causes.

**`scheduling.concurrent.*` cluster — fixed 2026-07-20 (4/4 classes).**
`ConcurrentTaskExecutorTests`, `DecoratedThreadPoolTaskExecutorTests`,
`ThreadPoolTaskExecutorTests`, `ThreadPoolTaskSchedulerTests` all now pass
100% (18/18, 14/14, 23/23, 40/40). Root cause: `native-collections` shadowed
`getCorePoolSize`/`getMaximumPoolSize`/`isShutdown`/`isTerminated`/
`shutdownNow` on the concrete class `java/util/concurrent/ThreadPoolExecutor`
unconditionally with CratonVM's synthetic 2-field executor layout, even for
REAL bytecode-constructed `ThreadPoolExecutor` instances (disambiguated only
by class name, which collides with the synthetic placeholder) — so
`setCorePoolSize()`/`setMaximumPoolSize()` mutations were silently ignored on
readback, and `shutdownNow()` interrupted workers but always returned an
empty list instead of draining `workQueue`, leaving queued `FutureTask`s
neither run nor cancelled (`future.get(timeout)` threw `TimeoutException`
instead of `CancellationException`). Fixed by routing real receivers through
the real JDK bytecode instead of the synthetic slots (see
`native-collections/src/lib.rs` `tp_is_real`), landed on `dev` at `2b41ba9b0`.
`scheduling.quartz.QuartzSupportTests` was investigated as a possible shared
residual but could not be verified either way: its module
(`spring-context-support`) doesn't compile against the shared
spring-framework checkout used for classpath generation (missing the
`org.springframework.aop.target` source package entirely, pre-existing and
unrelated to CratonVM) — left open, out of scope for the concurrent-cluster
fix.

**Groovy — 1/4 fixed.** `scripting.groovy.GroovyAspectTests` is now fixed;
`context.groovy.GroovyBeanDefinitionReaderTests` and
`scripting.groovy.GroovyScriptFactoryTests` are still hung (TIMEOUT), and
`web.servlet.view.groovy.GroovyMarkupViewTests` still FAILs (9/10).

**Resolved since 2026-07-17**: the `web.servlet.mvc.method.RequestMappingInfoHandlerMappingTests`
anomaly (previously FAIL despite 43/43 methods passing) is now a clean OK
(45/45) — whatever caused that status/method-count mismatch is gone.

## Full class list (66), by module

### Aop

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `aop.framework.autoproxy.BeanNameAutoProxyCreatorTests` | OK (2026-07-21 fix) | 9/9 | 5216ms |
| `aop.support.MethodMatchersTests` | OK (2026-07-21 fix) | 14/14 | 2463ms |

### Beans

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `beans.ConcurrentBeanWrapperTests` | OK (2026-07-21 fix) | 101/101 | 4027ms |
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | OK (2026-07-20 JIT fix) | 14/14 | 138189ms |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | FAIL (2026-07-20, major improvement, see above) | 31/34 | 324252ms |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | OK (2026-07-20 JIT fix, needs ~470s) | 47/47 | 466737ms |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | OK (2026-07-20 JIT fix, needs ~460s) | 44/44 | 456855ms |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT (severe perf defect — ~247x slower than HotSpot; free-list O(1) fix landed 2026-07-21, ~8.2% aggregate improvement so far, see above) | 0/0 | 350000ms |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | OK (2026-07-20 JIT fix) | 24/26 | 225204ms |
| `beans.factory.xml.XmlBeanFactoryTests` | FAIL | 85/95 | 85837ms |

### Context

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` | TIMEOUT | 0/0 | 120000ms |
| `context.annotation.ConfigurationClassPostConstructAndAutowiringTests` | OK (2026-07-21 SPR-8080 fix) | 2/2 | 829ms |
| `context.annotation.ConfigurationClassPostProcessorTests` | FAIL (2026-07-21 FactoryBean/CGLIB session: 4 genericsBasedInjectionWith* fixed, see notes) | 80/85 | ~25000ms |
| `context.annotation.Spr15275Tests` | OK (2026-07-21 FactoryBean-enhancement fix) | 6/6 | ~2500ms |
| `context.annotation.Spr6602Tests` | OK (2026-07-21 FactoryBean-enhancement fix) | 2/2 | ~4000ms |
| `context.aot.ApplicationContextAotGeneratorTests` | FAIL (2026-07-20, see caveat above — needs re-verify) | 16/40 | 389879ms |
| `context.groovy.GroovyBeanDefinitionReaderTests` | TIMEOUT | 0/0 | 120000ms |

### Core

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `core.GenericTypeResolverTests` | OK (2026-07-21 fix) | 25/25 | 352ms |
| `core.annotation.NestedRepeatableAnnotationsTests` | OK (2026-07-21 fix) | 12/12 | 230ms |
| `core.io.ResourceTests` | FAIL | 66/68 | 4689ms |
| `core.io.buffer.DataBufferTests` | TIMEOUT | 0/0 | 120000ms |
| `core.retry.RetryPolicyTests` | FAIL | 22/23 | 828ms |

### Expression

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `expression.spel.MethodInvocationTests` | OK (2026-07-21 fix) | 23/23 | 1427ms |
| `expression.spel.SpelCompilationCoverageTests` | OK (2026-07-21 fix) | 162/162 | 31969ms |

### Http

All 8 HTTP JSON/message-converter cluster classes fixed 2026-07-20 — see
"Notable clusters" above. Removed from this table.

### Jdbc

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jdbc.core.namedparam.BeanPropertySqlParameterSourceTests` | OK (2026-07-21 fix) | 10/10 | 547ms |
| `jdbc.core.namedparam.MapSqlParameterSourceTests` | OK (2026-07-21 fix) | 6/6 | 77ms |

### Jms

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jms.core.JmsTemplateTransactedTests` | OK (2026-07-21, side effect) | 52/52 | 24492ms |

### Jndi

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jndi.JndiObjectFactoryBeanTests` | FAIL | 24/25 | 2165ms |

### Orm

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` | OK (2026-07-20 JIT fix) | 8/8 | 136535ms |
| `orm.jpa.support.PersistenceInjectionTests` | FAIL | 26/27 | 11461ms |

### Scheduling

`scheduling.concurrent.*` (4 classes: `ConcurrentTaskExecutorTests`,
`DecoratedThreadPoolTaskExecutorTests`, `ThreadPoolTaskExecutorTests`,
`ThreadPoolTaskSchedulerTests`) fixed 2026-07-20 — see "Notable clusters"
above. Removed from this table.

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scheduling.quartz.QuartzSupportTests` | OK (2026-07-21, side effect) | 9/17 (8 skipped, 0 failed) | 6203ms |

### Scripting

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scripting.groovy.GroovyScriptFactoryTests` | TIMEOUT | 0/0 | 120000ms |

### Test

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `test.context.BootstrapUtilsTests` | OK (2026-07-21, side effect) | 23/23 | 1505ms |
| `test.context.aot.AotIntegrationTests` | FAIL (2026-07-20, now completes, see above) | 0/4 | 56296ms |
| `test.context.aot.TestClassScannerTests` | OK (2026-07-20 JIT fix) | 7/7 | 197691ms |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL (2026-07-20, improved, see above) | 2/4 | 148117ms |
| `test.context.bean.override.mockito.MockitoBeanByTypeLookupIntegrationTests` | OK (2026-07-23 fix) | 5/5 | 27473ms |
| `test.context.bean.override.mockito.constructor.MockitoBeanByTypeLookupForConstructorParametersIntegrationTests` | OK (2026-07-23 fix) | 6/6 | 16982ms |
| `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` | FAIL | 0/2 | 918ms |
| `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` | TIMEOUT | 0/0 | 120000ms |
| `test.context.testng.TestNGConcurrencyTests` | OK (2026-07-21, side effect) | 1/1 | 2439ms |
| `test.web.servlet.assertj.MockMvcTesterIntegrationTests` | FAIL | 72/74 | 58069ms |

### Util

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `util.CollectionUtilsTests` | OK (2026-07-21 fix) | 32/32 | 484ms |
| `util.StreamUtilsTests` | OK (2026-07-21 fix) | 11/11 | 1402ms |

### Web

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `web.client.RestClientIntegrationTests` | FAIL (2026-07-21, improved via side effect) | 227/230 | ~30000ms |
| `web.client.RestTemplateIntegrationTests` | FAIL (2026-07-21, improved via side effect) | 119/125 | ~20000ms |
| `web.context.request.RequestScopeTests` | OK (2026-07-21, side effect) | 7/7 | 1700ms |
| `web.reactive.function.client.WebClientIntegrationTests` | FAIL (2026-07-21, reconfirmed, 1 fail + 1 skip) | 168/170 | 23337ms |
| `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` | FAIL (2026-07-20, now completes, NEW bug found, see above) | 3/5 | 51669ms |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL (original CCE confirmed gone, new shared bug, see above) | 3/5 | 54239ms |
| `web.servlet.config.MvcNamespaceTests` | FAIL (2026-07-21, reconfirmed, not investigated) | 24/25 | 24938ms |
| `web.servlet.config.annotation.ViewResolutionIntegrationTests` | FAIL (2026-07-21, reconfirmed, not investigated) | 6/7 | 29415ms |
| `web.servlet.view.groovy.GroovyMarkupViewTests` | OK (2026-07-21, side effect) | 10/10 | 21827ms |
| `web.socket.messaging.StompWebSocketIntegrationTests` | FAIL (not re-verified 2026-07-21, host load prevented reconfirmation, see notes) | 14/16 | 105475ms |

## Raw data

- Original full-suite triage (177 bugs): 8 shards, dev `213d93ea`,
  binary `cratonvm-fullsuite-20260717.bin`, cross-referenced against HotSpot
  (516-class baseline + a fresh 129-class targeted HotSpot rerun).
- Reconfirmation rerun (this update): 4 shards, dev `8719dca85`, binary
  `cratonvm-fullsuite2-20260720.bin`, `LIST=` the exact 263 non-OK classes
  from the original run.
- Per-class FAILCAUSE and crash-log detail available in
  `/data/tmp/nonpassed263-s{0..3}/{failcauses,crashes}.log` on the Azure
  host at capture time.

## 2026-07-22 AOT follow-up  loader identity and synthetic StringBuilder fixes

Worktree: `/data/wt-aot-cluster-complete-20260721-019f873e` (branch
`codex/aot-cluster-complete-20260721-019f873e`), built against the real
JDK 25 Spring fixture. This follow-up intentionally remains **OPEN**: it
eliminated the previously dominant failures below, then exposed a later,
separate generated-AOT execution residual.

### Fixed in this follow-up

1. **Reference-array class identity now preserves the component's defining
   loader.** `Object.getClass()` on a reference array returns a synthetic
   descriptor mirror with the exact component loader, and
   `Array.newInstance(Class,...)` retains the supplied component
   `ClassId` instead of re-resolving it by binary name. This removes the
   stale app-loader array type from forked AOT loaders.

2. **Forked-loader symbolic static and interface resolution now stays in the
   initiating-loader namespace.** Static-owner caches, static invokes, and
   the pre-resolution part of `invokeinterface` use the same narrow
   `CompileWithForkedClassLoader` rule as ordinary symbolic resolution.
   This removed the old
   `MergedAnnotations$Search.withEnclosingClasses` /
   `SearchStrategy.TYPE_HIERARCHY` identity failure.

3. **Synthetic StringBuilder layout is now consistently routed to registered
   native methods for the direct JDK operations reached by JavaPoet.**
   CratonVM builders are `char[]/count`, whereas JDK 25 direct
   `StringBuilder` bodies use compact `byte[]/coder/count`. The routing
   covers constructor, append, charAt, delete, getChars, insert, length, and
   toString. The ArrayCopy bridge also recognizes the String.getBytes path
   through `AbstractStringBuilder.insert`.

   The final targeted addition was `StringBuilder.delete(int,int)`: Spring
   JavaPoet `LineWrapper` calls it directly, and omitting it left generated
   source corrupt (for example
   `registerAliases(DefaultListableBeanFactory beanFactory) {DefaultListableBeanFactory beanFactory) {`).

### Validation

* Unique binary: `/data/cratonvm-aotcomplete-019f873e-r10.bin`.
* `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests`:
  **5/5 OK** (was 3/5).
* `web.service.registry.ImportHttpServiceRegistrarTests`: **5/5 OK**
  (was 3/5).
* The previously failing `AotIntegrationTests` generation passed the
  malformed-source/parser point and compiled its generated test suite.
* As a control, the ordinary
  `TestBeanByNameLookupTestClassScopedExtensionContextIntegrationTests`
  run is **13/13 OK** on r10.

### Newly exposed residual  not fixed

After generated compilation,
`AotIntegrationTests.endToEndTestsForBeanOverrides` runs its 175-test
AOT-mode suite with **73 successful / 102 failed**. The ordinary direct run
of the same bean-override test class passes, so this is specific to
generated-AOT/forked-loader execution. The first shared symptom is Log4j
plugin configuration failing during reflective field/factory wiring with
`IllegalArgumentException: argument type mismatch` in
`PluginBuilder.injectFields`, followed by missing Logger/Root plugin
objects. This is consistent with another loader-identity/reflective
assignability boundary, but has not yet been localized enough for a safe
fix.

A 360-second full `AotIntegrationTests` r10 run advanced beyond this first
generated suite into later AOT processing, then hit the external timeout;
therefore neither it nor the broader AOT/TIMEOUT list should be marked
complete. Continue from the single-method probe
`/data/aotcomplete-probes-019f873e/KRunMethod` and log
`/data/aotcomplete-r10-singlemethod.log`.


## 2026-07-22 AOT follow-up 2 -- bean-override double-context-refresh root-caused, one contributing gap fixed

Worktree `/data/wt-aot-cluster-final-20260722` (branch
`fix/aot-cluster-final-20260722`). Follows directly from the "2026-07-22 AOT
follow-up" session above, which got `AotIntegrationTests` past generated
compilation but exposed `endToEndTestsForBeanOverrides` running its 175-test
generated-AOT suite at only 73/175. **This session confirms the Log4j
`PluginBuilder.injectFields` error documented there is a red herring** --
transient/host-load-dependent, reproduces on 1 of 10+ repeat runs of the
identical binary/command, and does NOT correlate with the actual 102 test
failures (a full run with the Log4j error absent still showed 102 failures).
Do not chase it further; if it recurs, just retry.

**Root cause of the real 102 failures (`@TestBean`/`@MockitoBean`/
`@MockitoSpyBean` overrides silently not applying) traced to a genuine
double-`ApplicationContext`-refresh bug**, isolated with a fast (~30-40s,
not 300s+) standalone repro
(`org.springframework.test.context.aot.AotIntegrationTests.runEndToEndTests`'s
API called directly against ONE minimal `@TestBean`-using fixture class, own
copy so diagnostics can be added without touching the shared checkout --
saved at `/data/tmp/aotcluster-final-repro/BeanOverrideProbe2.java` on the
Azure host). Confirmed via a `BeanFactoryAware`+`SmartInitializingSingleton`
probe bean that the SAME logical test gets its `DefaultListableBeanFactory`
refreshed TWICE -- once correctly (through
`BeanOverrideContextCustomizer.customizeContext()`, which calls the override
factory and `registerSingleton`), and a SECOND time on a completely
different `BeanFactory` instance that skips customization entirely (so
`preInstantiateSingletons()` creates the plain `@Bean`-defined production
value instead) -- and the SECOND, wrong context is what ends up bound to the
actual `@Test` method. Ruled out a `ConcurrentHashMap` correctness bug via
direct native tracing (`native_chm_get`/`native_chm_put_if_absent` in
`native-collections/src/lib.rs`) -- put/get round-trips perfectly for the
FIRST (correct) BeanFactory's `singletonObjects` map every time it was
queried; the bug is entirely about which `BeanFactory` instance ends up
wired to the test.

Traced (with the EXISTING `CRATONVM_DBG_DUPCLASS=1`/`CRATONVM_DBG_DUPCLASS_BT=1`
and `CRATONVM_FORNAME_TRACE=1` diagnostics, no new code needed to find this
part) to `AotTestContextInitializersFactory`/`AotTestContextInitializers`/
`AotMergedContextConfiguration`/`DefaultCacheAwareContextLoaderDelegate`
(all in `org.springframework.test.context.{aot,cache}`) repeatedly
re-resolving to FRESH Application-loader `ClassId`s instead of reusing the
`@CompileWithForkedClassLoader` fork's own already-loaded copy
(`classloading::class_manager::resolve_fast_path_class_id`'s documented
"always prefer Application over a lone UserDefined candidate" behavior).
Since `AotTestContextInitializersFactory`'s static double-checked-locking
cache (`private static volatile Map<...> contextInitializerClasses`) lives
on the CLASS object, a fresh `ClassId` means fresh (null) static state, so
`Class.forName`-based lookups re-run and can yield a different `Class`
object for the generated `TestContextNNN_ApplicationContextInitializer`.
`AotMergedContextConfiguration.hashCode()`/`.equals()` are defined purely in
terms of THAT `Class` object's identity (by design, matching real JDK
`Class` semantics) -- so a second, non-identical `Class` object busts
`DefaultContextCache.contextMap`'s cache-hit check on a later
`DefaultCacheAwareContextLoaderDelegate.loadContext()` call, causing a
second, uncustomized context load from scratch.

**One contributing gap found and fixed**: `spring_class_utils_for_name_impl`
(`native-builtins/src/phases_late.rs`, the native shortcut backing Spring's
`org.springframework.util.ClassUtils.forName`) only consulted an explicit
loader argument or the thread context classloader before falling through to
the global, loader-blind `ensure_class_initialized` path -- when NEITHER was
available (as in `GeneratedMapUtils.loadMap`'s `ClassUtils.forName(className,
null)` call with no TCCL set at that point), it never considered the
CALLING code's own defining loader, unlike the real `Class.forName(String)`
1-arg overload (`class_for_name_one_arg_caller_loader`, already correct).
Fixed by reusing that same caller-sensitive helper (promoted to
`pub(crate)`) as an additional fallback. Verified safe (`cargo test -p
cratonvm-vm --lib --release`: 2229 passed / 11 failed, byte-identical to the
pre-existing release-mode `lock_order`/`skip_list`/`tomcat_scanner`/
`elasticsearch_vector` baseline both before and after, confirmed via
`git stash` A/B; spot-checked `beans.factory.aot.
BeanDefinitionMethodGeneratorTests` 29/34 unchanged both sides -- NOTE this
count is lower than the 34/34 this class was verified at on 2026-07-21;
under this session's much heavier host load, ~20+ concurrent sessions,
`uptime` load average 20-24, this is far more likely re-triggered
cross-test-timing host contention (already documented for this exact class
in `[[spring-aot-cluster-loader-identity-fixes-20260715]]`'s "Round 7b")
than a regression -- re-verify on a quieter host before concluding either
way). Landed on `fix/aot-cluster-final-20260722` at `b0100920a`.

**This fix alone did NOT close the bean-override bug.** Re-running the
repro against the patched binary, `CRATONVM_DBG_DUPCLASS_BT=1` shows the
dominant remaining rejection now comes through a DIFFERENT, deeper path:
```
resolve_fast_path_class_id (classloading/src/class_manager.rs:2783)
  <- load_class_concurrent (vm/src/vm/vm_init.rs:3824)
  <- resolve_class_loader_aware (vm/src/runtime/interpreter.rs:17795)
  <- execute_instruction [New bytecode] (vm/src/runtime/interpreter.rs:15022)
```
i.e. a plain `new AotTestContextInitializers()` instruction (almost
certainly inside `DefaultCacheAwareContextLoaderDelegate`'s own
constructor) resolves its target class via `resolve_class_loader_aware` --
which the existing `is_compile_with_forked_class_loader` carve-out in
`should_use_loader_initiated_resolution` is SUPPOSED to make loader-aware
for exactly this scenario -- but still falls through to the global answer.
Since `should_use_loader_initiated_resolution` gates on
`defining_loader_for(referencing_class_id)` matching the fork's own loader,
and `DefaultCacheAwareContextLoaderDelegate` ITSELF is also among the
classes being repeatedly rejected/re-resolved per the same `DBG_DUPCLASS`
evidence, the most likely explanation is that the REFERENCING class (not
just its downstream reference) is itself sometimes resolved to the wrong
(Application-loader) copy at the moment this instruction executes -- a
strictly harder, more foundational problem than the single missing
fallback fixed above. **Recommended next step**: use
`CRATONVM_DBG_DUPCLASS_BT=1` against the fast repro
(`/data/tmp/aotcluster-final-repro/BeanOverrideProbe2.java` on the Azure
host, ~30-40s iteration, see the companion memory doc
`aot-beanoverride-double-context-refresh-rootcaused-20260722` for the full
probe-construction gotchas) to find WHERE
`DefaultCacheAwareContextLoaderDelegate`/`TestContextManager`'s own class
identity first goes wrong, then apply the same narrow, exact-`ClassId`-match
pattern used elsewhere in this file's history to that specific resolution
site.

**Still fully OPEN**: `AotIntegrationTests#endToEndTestsForBeanOverrides`
(73/175, dominated by this bug -- unchanged by this session's fix),
`ApplicationContextAotGeneratorTests`'s remaining 8 residuals (the `@Value`
field-injection gap, `"Hi null"` instead of `"Hi AOT World"`, is plausibly
the SAME double-refresh mechanism -- not yet cross-checked),
`test.context.aot.TestContextAotGeneratorIntegrationTests` (needs
re-baseline after this AND the prior session's fixes -- not done),
`beans.factory.aot.BeanRegistrationsAotContributionTests` (the separately-
tracked ~227x-vs-HotSpot interpreter throughput defect -- untouched this
session, needs dedicated profiling work, not a correctness bug).


## 2026-07-22 AOT follow-up 3 -- second contributing fix landed, real mechanism for the residual now identified

Same worktree/branch as follow-up 2. Landed a second, real, verified-safe
fix, then traced the remaining bug to its actual mechanism.

**Second fix**: `resolve_class_loader_aware`'s `user_loader` computation
(`vm/src/runtime/interpreter.rs`) re-queried `class_manager::get_loader_id`
for `referencing_class_id` even after `should_use_loader_initiated_resolution`
had already confirmed (via the separate `defining_loader_for` side table)
that the referencing class WAS defined by the fork loader -- when the two
sources disagreed, the gate's positive answer was silently discarded and
resolution fell through to the loader-blind global fast path. Fixed by
falling back to `defining_loader_for` (converted to a `ClassLoaderId` via
`loader_namespace_id`, promoted `pub`) on a disagreement instead of treating
it as "not a user loader". Landed at `f5831451a`, verified safe (`cargo test
-p cratonvm-vm --lib --release`: 2229/11, byte-identical baseline).

**Confirmed via `CRATONVM_DBG_LOADER_TRACE=1`** (widened its existing name
filter to include `AotTestContextInitializers`/`AotMergedContextConfiguration`/
`DefaultCacheAwareContextLoaderDelegate` -- zero new code needed beyond the
filter list, already-existing diagnostic) that BOTH fixes together now make
`AotTestContextInitializers`, `AotTestContextInitializersCodeGenerator`, and
`AotMergedContextConfiguration` resolve consistently and correctly through
`drive_defining_loader_load` (the fork's own `loadClass`) every time they
were referenced from the fork-loaded `DefaultCacheAwareContextLoaderDelegate`
(`ClassId(2486)`, `referencing_loader=Some(UserDefined(3))`) -- this specific
identity-instability layer is CLOSED.

**The double-context-refresh bug still reproduces** (confirmed against the
post-both-fixes binary). The loader trace shows why: partway through the
SAME test's lifecycle, a **second, genuinely different**
`DefaultCacheAwareContextLoaderDelegate` **object** comes into play --
`ClassId(6948)`, with `referencing_loader=Some(Application)` (not a
disagreement this time; `class_manager` and the side table AGREE this copy
is Application-loaded) -- and it resolves ITS OWN self-reference and
everything downstream (presumably `AotTestContextInitializers`/
`AotMergedContextConfiguration` too) via the ordinary global path, correctly
per ITS OWN loader identity, landing on the Application-loader's answer.
Since this is a **different delegate instance** (not just a different
`ClassId` for symbolically resolving the SAME logical singleton), it has its
own, independent `DefaultContextCache`, which naturally has never seen the
first delegate's customized context -- so it loads a fresh, uncustomized one
from scratch. Correlated by timing against the `BeanOverrideProbe2` probe's
own timestamped prints: the first (fork-scoped, `ClassId 2486`) delegate is
used for the correctly-customized context (matches
`BeanOverrideTestExecutionListener.prepareTestInstance` -> `injectFields` ->
`testContext.getApplicationContext()`, the FIRST of the two `loadContext()`
call sites `DefaultCacheAwareContextLoaderDelegate.loadContext()`'s own
Javadoc documents); the second (Application-scoped, `ClassId 6948`) delegate
appears shortly before the wrongful `bean1()` call, consistent with the
SECOND call site (the `@Test` method's own `ApplicationContext ctx`
parameter resolution).

**This reframes the remaining problem**: it is very likely NOT a
symbolic-class-resolution bug at all (the mechanism this doc's several AOT
sessions have been fixing all day) but a **`TestContextManager`/
`DefaultCacheAwareContextLoaderDelegate` object-instantiation duplication**
-- i.e. two DIFFERENT calls to `new DefaultCacheAwareContextLoaderDelegate()`
(or whatever constructs/caches the ONE that should be shared for a given
test) happening under two different loader contexts and NOT being
recognized as "the same test's infrastructure" by whatever caches/scopes
`TestContextManager` instances across a test's lifecycle -- plausibly
JUnit Jupiter's own `ExtensionContext.Store` (keyed by `Namespace` +
key objects, which can be subject to the exact same `Class`-identity-based
cache-key instability if `SpringExtension`'s own class resolves
inconsistently under the fork) rather than anything AOT-specific. **This is
a THIRD, distinct investigation layer** (JUnit's own extension-store
caching, not Spring's `DefaultContextCache` or CratonVM's `ClassId`
resolution) and was not pursued further this session -- flagged as the
concrete next step for whoever continues.

**Recommended next step**: widen `CRATONVM_DBG_LOADER_TRACE`'s filter
(already trivial to do, see this session's pattern) to also cover
`SpringExtension`/`TestContextManager`/`ExtensionContext` class names, and
add print instrumentation (via a custom probe fixture, NOT the shared
checkout) around `SpringExtension`'s `getTestContextManager(ExtensionContext)`
-- the actual JUnit-side store lookup -- to determine whether it's finding
two different `Store` instances, two different cached `TestContextManager`
values under the same key, or constructing a fresh one each time due to a
key-equality failure. The fast repro
(`/data/tmp/aotcluster-final-repro/BeanOverrideProbe2.java`, or its
non-forked sibling `BeanOverrideProbe3.java` used to confirm forking is
REQUIRED to trigger this at all -- both on the Azure host) remains the
fastest iteration path (~30-60s depending on host load and whether
`CRATONVM_DBG_LOADER_TRACE` is enabled, which adds significant overhead --
prefer `CRATONVM_DBG_DUPCLASS`/targeted name filters over blanket tracing).


## 2026-07-23 AOT follow-up 4 -- environment gotcha found, two more hypotheses on the double-refresh bug REFUTED

Worktree `/data/wt-aot-junitstore-20260723` (branch `fix/aot-junitstore-
20260723`, from `origin/dev` `cd23640d9`, which already contains every fix
from the prior three follow-ups above -- no new source changes landed this
session, only investigation). No subagents used, per this task's standing
instruction.

**Environment gotcha (not a CratonVM bug, but wasted significant time before
being found)**: the Azure host's PATH-default `java` is OpenJDK 21, which
lacks `java.lang.classfile` (JEP 484, stable since JDK 24). Spring 7's
`ClassFileMetadataReader` (a `src/main/java24` Multi-Release-JAR class,
confirmed genuinely MRJAR-packaged and genuinely selected correctly by
CratonVM) is reached by `ConfigurationClassParser.retrieveBeanMethodMetadata`
whenever a `@Configuration` class has **2+** `@Bean` methods -- which the
`BeanOverrideProbe2` repro's `Probe` diagnostic bean (added in the prior
session) pushed it into. Without a JDK 24+ boot image, this throws
`NoClassDefFoundError: java/lang/classfile/ClassFile`, which
`TestContextAotGenerator` (constructed with `failOnError=false`, the fast
repro's default) silently swallows into a WARN log, surfacing only as the
generic, misleadingly-familiar `IllegalStateException: Failed to load AOT
ApplicationContextInitializer class` -- easy to mistake for the DIFFERENT,
already-diagnosed "incomplete `@Nested` testClasses list" probe artifact from
the prior session's Finding 2. Confirmed via `failOnError=true` to see the
real `Caused by:` chain, and via a from-scratch `Class.forName("java.lang.
classfile.ClassFile")` micro-repro (throws `ClassNotFoundException` by
default, loads fine once pointed at a real JDK 24+ image). Fix: export
`CRATONVM_JAVA_HOME=/data/jdk25-real-20260717/jdk-25.0.3+9` (also reachable
via the shorter symlink `/home/victor/jdk25`, which other concurrent sessions
were already using via the `--java-home` CLI flag) before any ad-hoc
`KRun`/`KRunMethod` repro against Spring 7 fixtures. `classfile_api.rs`'s
`SyntheticStub` registrations are a deliberate non-implementation of JEP 484
(see its own doc comment) and do not make the class independently loadable
without a real backing classfile -- this is by design, not a gap to fix.
See `[[aot-cluster-missing-cratonvm-java-home-jdk25]]` (memory) for full
detail, including the open question (being checked as of this writeup) of
whether this same missing env var partially explains this doc's own
`ApplicationContextAotGeneratorTests`/`TestContextAotGeneratorIntegrationTests`
pass counts below.

**Double-context-refresh bug (`AotIntegrationTests#endToEndTestsForBeanOverrides`,
73/175): two more of the leading hypotheses from the 2026-07-22 write-ups
above are REFUTED, with hard evidence, once the environment gotcha was
fixed and the fast repro reached the real bug again**:
- **NOT JUnit `ExtensionContext.Store` returning a different cached
  `TestContextManager`.** A same-package (`org.springframework.test.context.
  junit.jupiter`) read-only diagnostic extension, registered via
  `@ExtendWith` alongside `@SpringJUnitConfig` on the repro fixture, printed
  `identityHashCode` of the Store's cached `TestContextManager` at
  `beforeAll`/`postProcessTestInstance`/`beforeTestExecution`/
  `afterTestExecution` -- IDENTICAL at all four points, bracketing both the
  correct and the wrong `Probe` lifecycle. There is exactly one
  `TestContextManager` for the whole test.
- **NOT a residual `ClassId`/loader-identity instability for the generated
  initializer class.** The same diagnostic called the public
  `new AotTestContextInitializers().getContextInitializerClass(testClass)`
  at all four points -- identical `Class` object (identity, `.hashCode()`,
  `.equals(self)`) every time, across 3 repeat runs. This independently
  corroborates `[[spring-aot-cluster-loader-identity-fixes-20260715]]`'s
  claim that `b0100920a`/`f5831451a` actually closed this layer.
- **NOT `AotDetector.useGeneratedArtifacts()` flipping.** Same diagnostic,
  also printed at all four points: `true`/`true`/`true` throughout, so
  `DefaultCacheAwareContextLoaderDelegate.replaceIfNecessary` is confirmed
  taking the `AotMergedContextConfiguration`-wrapping branch consistently.
- **NOT a generic `LinkedHashMap`(access-order)+`Collections.synchronizedMap`
  correctness bug** (`DefaultContextCache.contextMap`'s exact shape,
  untested by the earlier `ConcurrentHashMap`-only ruling-out for
  `singletonObjects`). A standalone, Spring-free probe reproducing the exact
  key shape (`equals`/`hashCode` delegating to a wrapped `Class`, matching
  `AotMergedContextConfiguration` exactly) with unrelated noise puts/gets in
  between showed correct cache-hit behavior on CratonVM.

Every input to the delegate's cache-hit decision that is observable from
outside the `org.springframework.test.context.cache` package is now
confirmed stable and correct, yet the bug still reproduces identically (two
`Probe.setBeanFactory`/`afterSingletonsInstantiated` cycles with different
`beanFactory` identityHashes, second one uncustomized). **Still fully OPEN**
-- the next step requires either instrumenting `DefaultContextCache.
contextMap`'s actual `get`/`put` calls directly (risky: needs a temporary
patch to the shared `spring-framework-recheck` checkout, or a
classpath-shadowing private copy of `spring-test`) or Rust-level
`CRATONVM_DBG_LOADER_TRACE`/`DBG_DUPCLASS_BT` tracing specifically checking
whether `DefaultContextCache`'s own class (and hence its static
`defaultContextCache` singleton) is itself duplicated, which was never
directly checked (only the delegate INSTANCE's stability was, which is
confirmed but doesn't rule this out). See
`[[aot-double-context-refresh-not-junit-store-not-classid]]` (memory) for
the full diagnostic-by-diagnostic writeup and reusable probe file locations.

**`ApplicationContextAotGeneratorTests` re-verified with `CRATONVM_JAVA_HOME`
correctly set: 33/40 (unchanged from the documented `--nojit` baseline)** --
the missing-env-var hypothesis does NOT explain this class's residuals; its
official-runner count was already accurate. The 5 confirmed failures are all
the already-documented CGLIB-proxy/duplicate-`ClassId` family
(`processAheadOfTimeWithExplicitResolvableType` `AotBeanProcessingException`,
`...WhenHasCglibProxyWriteProxyAndGenerateReflectionHints`,
`...WhenHasCglibProxyUseProxy`, `...UsesCglibClassForFactoryMethod`
`CompilationException`, `...WhenHasCglibProxyWithAnnotationsOnTheUserClasConstructor`
`CompilationException`) from the "2026-07-21 late session" unifying
root-cause finding above (still NOT fixed, needs a dedicated session per that
write-up). **Good news**: `processAheadOfTimeWhenHasCglibProxyAndMixedAutowiring`
(the `@Value` field-injection gap this doc's follow-up-2 section suspected
might share the double-refresh mechanism) is **no longer failing** -- it
passed in this re-run, so that specific residual is resolved (by one of
today's earlier loader-identity fixes, not cross-checked which one
specifically) and the "plausibly the SAME mechanism -- not yet cross-checked"
question is moot.

`test.context.aot.TestContextAotGeneratorIntegrationTests` re-baseline is
**still blocked, but now by a newly-confirmed-reproducible (2/2) hang/crash,
not by the missing-JAVA_HOME environment issue**: both this session's attempts
(one under heavy host load, `uptime` ~20+, one after load dropped to ~2-11)
died identically, ~1-2 minutes into the run, DURING JUnit test *discovery*
(stack bottoms out at `KRunMethod`/`KRun2.main`'s `LauncherFactory.create().
execute(...)` call, before any `@Test` method actually starts) with:
```
<clinit> failed — wrapping in ExceptionInInitializerError class=groovy/lang/GroovySystem
  cause=java/lang/NullPointerException Cannot read the array length because "<local2>" is null
```
No further output, no clean Java stack trace, no `[cratonvm] main-vm run()
returned Err` exit line either -- the process goes silent and is eventually
killed by the harness's `timeout` wrapper, consistent with a silent hang
rather than a caught-and-reported exception. Not yet root-caused (would need
`CRATONVM_DBG_ATHROW=1` or similar to find why the wrapped
`ExceptionInInitializerError` doesn't propagate/get caught normally, and why
GroovySystem's clinit is reached at all during plain JUnit discovery of a
Spring Framework test class with no declared Groovy dependency). Flag as a
NEW, distinct blocker for whoever continues -- do not conflate with the
already-closed loader-identity family above.


## 2026-07-23 AOT follow-up 5 -- double-context-refresh ROOT-CAUSED AND FIXED

Same session as follow-up 4, continuing directly from its refuted
hypotheses. Worktree `/data/wt-aot-junitstore-20260723` (branch
`fix/aot-junitstore-20260723`). No subagents used, per this task's standing
instruction.

**Method**: since every externally-observable input to
`DefaultCacheAwareContextLoaderDelegate`'s cache-hit decision was already
proven stable (follow-up 4), the delegate itself was wrapped directly.
Subclassed `DefaultTestContextBootstrapper`, overrode
`getCacheAwareContextLoaderDelegate()` to return a logging wrapper around the
real delegate, and registered it via `@BootstrapWith(LoggingBootstrapper.class)`
on the `BeanOverrideProbe2` fixture (own new probe file, not the shared
`spring-framework-recheck` checkout). **Every single `loadContext`/
`isContextLoaded` call for the entire run returned the SAME correct
`ApplicationContext`** -- yet `Probe.setBeanFactory` still fired twice with
two different `BeanFactory` identities. This proves the second, wrong
context is never routed through `CacheAwareContextLoaderDelegate` at all.

Widening the probe's own stack-trace capture (6 to 60 frames) for the wrong
`bean1()` call showed the full chain ending in:
```
DefaultCacheAwareContextLoaderDelegate.loadContext   <- the RAW class, not the wrapper
DefaultTestContext.getApplicationContext
SpringExtension.getApplicationContext
ParameterResolutionUtils.resolveParameter
```
-- i.e. a genuinely SECOND, independent `TestContext`/`TestContextManager`
that never went through the wrapped bootstrapper (confirmed:
`getCacheAwareContextLoaderDelegate()` printed only 3 times all run, none
near this second construction). `CRATONVM_DBG_DUPCLASS=1
CRATONVM_DBG_DUPCLASS_BT=1` then showed a correlated rejection cluster for
EXACTLY `SpringExtension`, `TestContextManager`, `BootstrapUtils`,
`DefaultTestContextBootstrapper`/`AbstractTestContextBootstrapper`/
`TestContextBootstrapper` -- all "a SEPARATE ClassId will be created under
Application" -- with the `TestContextManager` rejection's backtrace bottoming
out in `native-builtins/src/lib.rs`'s `spring_extension_get_application_context`
/`native_spring_extension_resolve_parameter`, NOT real Spring bytecode.

**Root cause**: `SpringExtension.resolveParameter` is intercepted by a
single, globally-registered native override
(`native_spring_extension_resolve_parameter`). Its helper,
`spring_extension_get_application_context`, invoked the real
`SpringExtension.getApplicationContext(ExtensionContext)` bytecode via
`ctx.invoke_special("org/springframework/test/context/junit/jupiter/SpringExtension", ...)`
-- a NAME-based lookup with no `referencing_class_id` (this native trampoline
has no bytecode frame of its own to derive one from), so class resolution
fell through to the ordinary loader-BLIND `load_class_concurrent` path, which
(per its long-documented behavior) prefers the Application-loader copy
whenever the delegation chain can also serve the name. Under
`@CompileWithForkedClassLoader`, the fork has its OWN already-loaded copy of
`SpringExtension` -- but this native ignored it and always resolved a fresh
Application-loader copy. Since `SpringExtension.getTestContextManager` keys
its JUnit `ExtensionContext.Store` lookup on `Namespace.create(SpringExtension.class)`
(identity-based), the wrong copy's `Class` object is a different `Namespace`
identity, so `store.computeIfAbsent(testClass, TestContextManager::new, ...)`
misses and builds a SECOND, independent `TestContextManager` with its own
un-customized `ApplicationContext` (`BeanOverrideContextCustomizer` never
ran for it) -- exactly the context that ends up bound to the `@Test`
method's `ApplicationContext`-typed parameter, since parameter resolution is
precisely where this native runs.

**Fix**: `spring_extension_get_application_context` now resolves the
current test class (`extension_context.getRequiredTestClass()`), its
`ClassId` (`lang_class::mirror_class_id`), and that class's defining loader
(`ctx.loader_id_of_class`). If that loader is user-defined
(`loader_id >= 3`) and it has already loaded its own copy of
`SpringExtension` (`ctx.class_id_defined_by_loader_exact`), invokes
`getApplicationContext` on THAT exact `ClassId` via `ctx.invoke_by_class_id`
instead. Falls through unchanged to the original by-name `invoke_special`
in every other case -- purely additive, zero behavior change for the vast
majority of test classes that never use `@CompileWithForkedClassLoader`.

**Verified**: the `BeanOverrideProbe2` fast repro now passes -- exactly ONE
`Probe.setBeanFactory`/`afterSingletonsInstantiated` cycle fires (was two),
the `@Test` method's `ApplicationContext` parameter resolves to the correct,
customized context, `ctx.getBean("field")` and the injected `@TestBean`
field both read `"fieldOverride"` (were `"prod"`/mismatched before this
fix). `cargo test -p cratonvm-vm --lib --release`: **2229 passed / 11
failed**, byte-identical to the documented pre-existing baseline (the
`lock_order`/`skip_list`/`tomcat_scanner`/`elasticsearch_vector` family) --
zero regressions.

**Still open / not done this session**: re-running the FULL
`AotIntegrationTests#endToEndTestsForBeanOverrides` 175-test suite (only the
fast single-fixture repro was verified, not the full suite -- the mechanism
is proven fixed but the aggregate pass count wasn't re-measured, needs a
300-400s+ run); `ApplicationContextAotGeneratorTests`'s remaining 5
CGLIB/duplicate-`ClassId` failures (confirmed unrelated to this fix, same
"2026-07-21 late session" family, a different native/bytecode call site);
`TestContextAotGeneratorIntegrationTests`'s `GroovySystem` clinit hang
(unrelated, not root-caused); and a worthwhile follow-up audit of whether
OTHER native trampolines that call `ctx.invoke_special`/`ctx.invoke` by name
on Spring TestContext Framework classes have the same loader-blindness (this
fix only touches the one call site that was actually proven to matter here).
See `[[aot-double-refresh-springextension-loader-blind-fix]]` (memory) for
the full diagnostic-by-diagnostic writeup.


## 2026-07-23 AOT follow-up 6 -- second loader-blindness call site fixed, follow-up 5's own next-steps resolved

Same worktree/branch as follow-up 5 (`/data/wt-aot-junitstore-20260723`,
`fix/aot-junitstore-20260723`). No subagents used.

**Second loader-blindness call site found and fixed.** The same native
family's constructor-injection branch (`native_spring_extension_resolve_parameter`,
reached when a test class's `@Nested`-hierarchy constructor needs
`findProperlyScopedExtensionContext`) also called `SpringExtension` by name
via `ctx.invoke_special`, with the identical potential to land on the
Application-loader copy under `@CompileWithForkedClassLoader`. Extracted a
shared `spring_extension_invoke_special_anchored_on_test_class` helper (the
same anchoring logic as follow-up 5's fix) and reused it for both call
sites. Commit `852d3cbd6`, merged and pushed to `origin/dev` at `d6563bd8c`.
`cargo test -p cratonvm-vm --lib --release`: 2230 passed / 11 failed, same
11 pre-existing baseline failures, zero regressions.

**Full 175-test `endToEndTestsForBeanOverrides` re-verified: 73/175 ->
~158/175.** Ran the actual `KRunMethod ... AotIntegrationTests
endToEndTestsForBeanOverrides` single-method probe (not just the fast
`BeanOverrideProbe2` fixture) with both fixes applied. Result:
`MultipleFailuresError: Test execution failures (17 failures)` -- down from
the documented 102. The remaining 17 cluster into two DISTINCT, UNRELATED
families, neither a loader-identity issue:
- The majority: `@MockitoBean`/`@MockitoSpyBean` "by name" lookup for
  CONSTRUCTOR-injected parameters (`MockitoBeanByNameLookupForConstructorParametersIntegrationTests`,
  `MockitoSpyBeanByNameLookupForConstructorParametersIntegrationTests`,
  `MockitoBeansByNameIntegrationTests`) fail with `No qualifying bean...
  expected single matching bean but found N`, where the listed candidate
  names show the override bean sitting ALONGSIDE the original(s) it should
  have replaced -- a bean-override-not-replacing-original bug specific to
  constructor injection, not investigated further.
- A smaller family: plain `AssertionFailedError: expected: null but was:
  ""` -- not yet isolated to a specific test class.
See `[[aot-endtoend-beanoverrides-73-to-158-of-175]]` (memory) for the full
breakdown, including which log4j-noise lines to ignore.

**`ApplicationContextAotGeneratorTests`'s residuals RE-CHARACTERIZED -- NOT
the duplicate-ClassId/CGLIB-naming family this doc's "2026-07-21 late
session" assumed.** Re-examined the actual exception text (not just the
class/method names) for the 2 `CompilationException` failures
(`processAheadOfTimeUsesCglibClassForFactoryMethod`,
`...WithAnnotationsOnTheUserClasConstructor`). Both are REAL JAVAC (`com.
sun.tools.javac.jvm.ClassReader`'s own diagnostic format) reporting
`org.springframework.beans.factory.aot.AutowiredArguments` as a "bad class
file... truncated" -- but this javac instance runs AS INTERPRETED BYTECODE
INSIDE CratonVM (`TestCompiler.forSystem()`'s in-process compile), so every
file read it performs goes through CratonVM's own native I/O. Two
independent tests ruled out "the jar is just corrupted": the existing jar
entry passes external Python `zipfile` validation (correct declared size,
no CRC error) AND a from-scratch recompile with real JDK 25's own `javac`
(prepended to the classpath, confirmed via the changed byte offset in the
error message that THIS file was actually being read) is STILL reported
"truncated," this time at ITS OWN different real size. Truncated-at-exactly-
its-own-real-length, reproduced across two differently-sized files compiled
from the same source, points at a **classfile `TypeAnnotations`-attribute
parsing bug in CratonVM's own reader** rather than file corruption:
`AutowiredArguments` is a `@FunctionalInterface` with JSpecify `@Nullable`
TYPE_USE annotations on a generic method return type and an array return
type -- exactly the structurally-complex `type_path` cases this attribute
exists to encode. Possibly a not-yet-covered edge case of the existing
JSpecify TYPE_USE fix family. NOT fixed this session (core classloading/
reader-crate work, out of scope for the narrowly-scoped native-trampoline
fixes landed today) -- see
`[[aot-cglib-residuals-not-classid-its-typeannotations-classfile-bug]]`
(memory) for the full evidence chain and recommended isolated repro.
`processAheadOfTimeWithExplicitResolvableType`'s `AotBeanProcessingException`
and the two `AssertionError`/`AssertionFailedError` failures for this class
remain uncharacterized.

**`TestContextAotGeneratorIntegrationTests`'s hang: genuine slowness inside
Groovy's own runtime bootstrap, NOT conclusively a deadlock -- host
contention is a major unresolved confound.** `--stack-dump-on-timeout`
(a real CratonVM flag) caught the main thread genuinely busy inside real
javac's `Attr`/`DeferredAttr` in one run, and inside `org/codehaus/groovy/
reflection/stdclasses/CachedSAMClass.hasUsableImplementation` <-
`CompileWithForkedClassLoaderClassLoader.loadClass` in another, with
`CRATONVM_DBG_ATHROW=1` additionally showing the SAME thread had recently
been deep in Groovy's own `MetaClassRegistryImpl.<init>` -> `registerMethods`
-- eagerly registering every "Default Groovy Method," a well-known-heavy,
one-time Groovy runtime bootstrap step, not a lock-wait. However, a full
30-minute run (`timeout 1800`) under severe host load (`uptime` load
average 19-23, 40+ concurrent users) still never completed, frozen at the
exact same point every time. This is NOT yet conclusively distinguished
from "just extremely slow under this much contention" -- needs a re-test
on a quiet host (load average < 2) before concluding either way; if it
still doesn't finish in ~10x the normal ~490s AOT-suite runtime there,
that's real evidence of a genuine hang. See the updated
`[[testcontextaotgeneratorintegrationtests-groovysystem-clinit-hang]]`
(memory).

**Native-trampoline loader-blindness audit: done, no further fixes
warranted.** Grepped `native-builtins/src/lib.rs` for every `ctx.
invoke_special`/`ctx.invoke` by-name call site. Beyond the two
`SpringExtension` ones fixed above, the only other Spring-TestContext-
adjacent one is `ParameterResolutionDelegate.resolveDependency`
(spring-beans) -- left alone: it is a stateless utility with no `Namespace`/
`Class`-identity-keyed caching downstream, so a wrong-loader-copy
resolution there has no observable behavioral consequence, unlike
`SpringExtension` whose own static state IS keyed on its `Class` object
identity. Everything else found (`org/apache/maven/surefire/booter/
ForkedBooter`, `java/io/OutputStreamWriter`/`PrintWriter`, `java/util/
concurrent/Semaphore`, `org/apache/activemq/broker/region/AbstractRegion`)
belongs to unrelated subsystems never subject to
`@CompileWithForkedClassLoader` duplication.


## 2026-07-23 AOT follow-up 7 -- MockitoBean constructor-param native shortcut restored, sixth JIT residual (ClassReader.readInnerClasses) root-caused and fixed

Worktree `/data/wt-aot-final-close-20260723` (branch `fix/aot-final-close-20260723`,
from `origin/dev` `893ddbc73`). No subagents used, per this task's standing
instruction. Pushed to `origin/dev` at `9fb1b8749`.

### Fix 1: `@MockitoBean`/`@MockitoSpyBean` constructor-parameter by-name lookup

**Root cause found**: `native_spring_extension_resolve_parameter`
(`native-builtins/src/lib.rs`) -- the same native trampoline
follow-up 5/6 above fixed for loader-blindness -- is a full native
reimplementation of `SpringExtension.resolveParameter`, not a thin
delegate. Its doc comment claimed "Spring Framework 7.0.7 delegates every
parameter shape directly to `ParameterResolutionDelegate`... the former
ApplicationContext/BeanOverride branches drifted from Spring and linked a
removed `SpringExtension.isBeanOverride` method" -- **this was simply
wrong**. The actual, current `spring-framework-recheck` checkout's
`SpringExtension.java` (verified by reading the source directly) still has
`isBeanOverride(Parameter)` and, in `resolveParameter`, a direct
`applicationContext.getBean(handler.getBeanName())` shortcut ahead of the
`ParameterResolutionDelegate` fallback, for any parameter whose
`BeanOverrideHandler.getBeanName()` resolves non-null. The native mirror
never had this branch (or had it removed at some point based on a stale
checkout), so it *always* fell through to the ambiguous by-type
`ParameterResolutionDelegate.resolveDependency` path.

**Symptom**: any `@MockitoBean`/`@MockitoSpyBean`-annotated *constructor*
parameter whose override bean name doesn't happen to equal the parameter's
own name (or carry an explicit `@Qualifier`) throws
`NoUniqueBeanDefinitionException: ... expected single matching bean but
found N: <every sibling override's bean name>` -- since all sibling
override beans (mocks/spies registered by the SAME `BeanOverrideBeanFactoryPostProcessor`
pass) share the exact declared type. Root-caused with a sequence of
isolated probes proving each individual mechanism (`MergedAnnotations`
meta-annotation scanning, `@AliasFor` resolution, `Parameter` reflection,
`BeanOverrideUtils.resolveHandlerForParameter`, `DefaultListableBeanFactory`
disambiguation) byte-identical between CratonVM and real JDK 25 in
isolation -- the actual divergence only showed up by adding an
unconditional canary exception at the top of a *locally patched copy* of
`SpringExtension.resolveParameter`'s Java source: it never fired under
CratonVM (proving the real bytecode body never runs), while the exact same
build fired it immediately under real JDK. That, plus a full unelided
stack-trace dump of the actual failure (no `SpringExtension` frame at all,
straight from `ParameterResolutionUtils` into `ParameterResolutionDelegate`),
pointed at a same-class *native* override rather than a Java-level bug --
confirmed by finding the `registry.register("...SpringExtension",
"resolveParameter", ...)` entry in `native-builtins/src/lib.rs`.

**Fix**: added the same `BeanOverrideUtils.resolveHandlerForParameter` +
`handler.getBeanName()` + direct `applicationContext.getBean(name)`
shortcut to the native, purely additive (falls through to the existing
`ParameterResolutionDelegate.resolveDependency` call unchanged when no
override name resolves).

**Verified**: `MockitoBeanByNameLookupForConstructorParametersIntegrationTests`
7/7 (was 0/7), `MockitoSpyBeanByNameLookupForConstructorParametersIntegrationTests`
5/5, `MockitoBeansByNameIntegrationTests` 1/1 -- all three of the
previously-documented "Family A" residuals from
`[[aot-endtoend-beanoverrides-73-to-158-of-175]]` now fully pass.

### Fix 2 (bonus, found while chasing Fix 1): `ClassFinder.fillIn` fifth JIT residual

While bisecting a *different* NPE hit during repeated in-process javac
compiles (see Fix 3 below), found that `com.sun.tools.javac.code.
ClassFinder.fillIn` -- the method `ClassFinder.complete` (already
skip-listed as `ClassFinderComplete`) itself calls to do the actual symbol
completion -- independently miscompiles once IT crosses its own JIT
tier-up threshold, throwing a raw `NullPointerException` from inside real
javac (`Types.unboxedType -> ClassFinder.complete -> fillIn`) while
attributing a `new Object[]{...}` array literal, surfacing as javac's own
internal-compiler-error report. `ClassFinderComplete`'s own doc comment
had explicitly bisect-RULED-OUT `fillIn` for the two symptoms known at the
time -- that ruling was correct for those symptoms, but `fillIn` has its
own, separate JIT eligibility from its caller and can still miscompile on
its own under a long enough loop. Added `SkipReason::ClassFinderFillIn`.

### Fix 3: `ClassReader.readInnerClasses` -- sixth JIT residual, root cause of the long-standing "AutowiredArguments...truncated" bug

This closes the residual flagged in
`[[aot-cglib-residuals-not-classid-its-typeannotations-classfile-bug]]`
(memory) and the "2026-07-21 late session" table row above for
`ApplicationContextAotGeneratorTests`'s `processAheadOfTimeUsesCglibClassForFactoryMethod`
/ `...WithAnnotationsOnTheUserClasConstructor`.

**The original hypothesis was a red herring.** That memory entry suspected
a `TypeAnnotations`-attribute-length parsing bug in CratonVM's own
classfile reader (`reader/src/attribute.rs`), since the failing class
(`AutowiredArguments`) is a `@FunctionalInterface` with JSpecify
`@Nullable` TYPE_USE annotations on a generic method return type and an
array return type -- exactly the structurally-complex `type_path` cases
that attribute exists to encode.

**What actually happened**: built a Spring-free, ~30-line standalone repro
(a small JSpecify-`@Nullable`-annotated `@FunctionalInterface` +
`ToolProvider.getSystemJavaCompiler().getTask(...).call()` looped ~40x in
one process) and confirmed it reproduces the exact "`bad class file...
class file truncated at offset N`" symptom deterministically at iteration
38, and confirmed via `--nojit` (all iterations pass) that this is a
genuine JIT miscompilation, not a reader-crate bug at all -- consistent
with the SAME family as `ClassReaderReadClass`/`ClassFinderComplete`/
`ClassFinderFillIn` above, just a fifth-become-sixth member nobody had
isolated yet.

Bisected via `CRATONVM_JIT_BISECT_SKIP` binary search across every method
on `com.sun.tools.javac.jvm.ClassReader` (~70 candidates enumerated via
`javap -p com.sun.tools.javac.jvm.ClassReader` against the real JDK 25
install) -- deliberately ruled out every TYPE_ANNOTATIONS/signature/
attribute-value-reading method FIRST (`readTypePath`, `readPosition`,
`readTypeAnnotation`, `attachTypeAnnotations`, `sigToType`/`sigToTypeParams`/
`classSigToType`, `readAnnotations`/`readCompoundAnnotation`/
`readAttributeValue`, `nextByte`/`nextChar`/`nextInt`, etc. -- all
skip-listed together, zero effect, exact same failure at the exact same
offset) before finding it via a 25-then-12-then-6-then-3-then-1 binary
search: **`ClassReader.readInnerClasses`** (this exact method alone) is
sufficient.

**Mechanism**: `readInnerClasses` reads an entry count via `nextChar()`
then loops that many times, each iteration doing 4 more `nextChar()`
calls plus conditional `enterClass`/`enterMember`/`ClassType.
setEnclosingType` calls -- a moderately complex loop that JIT-miscompiles
its own trip-count/cursor-advance handling once tier-compiled, over- or
under-consuming bytes from the shared `ClassReader.bp` buffer-position
field. Since `bp` is shared mutable state on the `ClassReader` instance
(reused across the whole `javac` invocation, not per-classfile), the
corruption doesn't necessarily surface on the SAME classfile being read --
a control run with the pre-fix binary showed the "truncated" error landing
on **`java.lang.Integer.class`** (an entirely unrelated JDK bootstrap
class, autoboxed from `new Object[]{1, 2}` in the trivial repro's user
code), not `AutowiredArguments` at all. `AutowiredArguments` was simply
one of many `InnerClasses`-bearing classfiles read in the AOT test
scenario's much longer compile sequence, unlucky enough to be read right
after `readInnerClasses` had crossed its JIT tier-up threshold -- the
TYPE_USE annotations on it were coincidental, not causal.

Added `SkipReason::ClassReaderReadInnerClasses`.

**Verified against the real `ApplicationContextAotGeneratorTests`**: both
previously-`CompilationException` tests no longer hit ANY compile failure.
`...WithAnnotationsOnTheUserClasConstructor` now passes outright.
`processAheadOfTimeUsesCglibClassForFactoryMethod` now reaches (and only
fails on) the SEPARATE, already-documented CGLIB duplicate-`ClassId`
family (`IllegalArgumentException: class ... is not an enhanced class`) --
a distinct, unrelated bug this session did not attempt to fix. Full-class
re-run: 34/40 (was 34/40 before this session's fixes too, but with a
DIFFERENT set of 5 failures -- the two truncation-based ones are gone,
replaced by the CGLIB "not enhanced" one plus 3 others this session did
not further chase: `processAheadOfTimeWithExplicitResolvableType`'s
`AotBeanProcessingException` on bean `hierarchyBean`,
`processAheadOfTimeWhenHasCglibProxyWriteProxyAndGenerateReflectionHints`'s
`Expecting actual not to be null`, and `processAheadOfTimeWhenHasCglibProxyUseProxy`'s
`expected: "Hello0 World" but was: "Hello1 World"`, plus
`processAheadOfTimeWhenHasAutowiringOnUnresolvedGeneric`'s
`AutowiredGenericTemplate` mismatch -- none characterized further this
session, flagged as the next targets for whoever continues).

Also merged in a concurrent, independent fix from another session
(`ClassSymbolComplete` -- `Symbol$ClassSymbol.complete` operand-stack
underflow, found via H2 stored-procedure `CREATE ALIAS ... AS $$`) that
turned out to be the SAME symptom this session separately observed as a
NEW `ApplicationContextAotGeneratorTests` failure
(`processAheadOfTimeWhenHasSimpleBean`'s "operand stack underflow") --
already fixed on `origin/dev` by the time this session's branch merged, no
duplicate work needed.

`cargo test -p cratonvm-vm --lib --release`: 2230 passed / 13 failed, same
pre-existing `lock_order`/`skip_list`/`tomcat_scanner` release-mode
baseline family throughout (before, after each fix, and after the final
merge to `origin/dev`), zero regressions at any point.

**Still open for whoever continues** (see `[[aot-endtoend-beanoverrides-73-to-158-of-175]]`
and this doc's earlier sections for full context):
1. "Family B" of the `endToEndTestsForBeanOverrides` 175-test suite (a
   smaller cluster of plain `AssertionFailedError: expected: null but was:
   ""`) -- still not isolated to specific test classes; the full 175-test
   aggregate run needs several GB of heap (`--Xmx 4g` recommended) and
   repeatedly hit host-level OOM/kill under this session's shared-host
   contention, so a clean full-suite count was not obtained this session.
2. `ApplicationContextAotGeneratorTests`'s remaining 6 residuals (listed
   above) -- 5 distinct, uncharacterized failure modes plus the
   already-known CGLIB duplicate-`ClassId` one.
3. `TestContextAotGeneratorIntegrationTests`'s `GroovySystem` hang/slowness
   -- still needs a re-test on a genuinely quiet host (load average < 2) to
   distinguish real hang from host-contention artifact; this session's host
   oscillated between load 1 and load 100+ repeatedly and was never quiet
   for long enough to attempt it.

## 2026-07-24 AOT follow-up 8 -- three more residuals closed (34/40 -> 36/40), two CGLIB cross-test failures root-caused but not yet fixed, new lambda-singleton gap found

Worktree `/data/wt-aot-residuals3-20260723` (branch
`fix/aot-residuals3-20260723`, from `origin/dev` `1dae989b1`, merged with
`origin/dev` `d58e8ea1e` mid-session with no conflicts). No subagents used,
per this task's standing instruction. Pushed directly to `origin/dev` at
`ce0804f81` (fast-forward, `d58e8ea1e..ce0804f81`) — the shared main
checkout at `/data/data/cratonvm` had uncommitted changes belonging to
another concurrent session, so the merge-and-push was done entirely from
this session's own worktree instead of touching the shared checkout.

### Fix 1: `ClassReader.readAttrs` -- SEVENTH JIT-miscompile residual in the repeated-in-process-javac family

Same family as `ClassReaderReadClass`/`ClassFinderComplete`/
`ClassFinderFillIn`/`ClassReaderReadInnerClasses`/`ClassSymbolComplete`
(see follow-up 7 and earlier entries above). New symptom: `bad class
file... bad signature: "ourceFile"` (the leading `S` of `SourceFile` lost
-- a `ClassReader.bp` cursor desync) while compiling AOT-generated sources
against classpath `.class` files, reproduced deterministically via
`ApplicationContextAotGeneratorTests$ConfigurationClassCglibProxy
.processAheadOfTimeWhenHasCglibProxyUseProxy` even with every previously-
known culprit already interpreted. Root-caused to `ClassReader.readAttrs`
-- the attribute-dispatch loop `readClassAttrs`/`readMemberAttrs` both
delegate straight into (read a count via `nextChar()`, then loop reading a
name-index `nextChar()` + length `nextInt()` per entry) -- via the
established methodology: `--nojit` control (pass), `CRATONVM_JIT_DENY=
com/sun/tools/javac/jvm/ClassReader` (whole class: pass), then
`CRATONVM_JIT_BISECT_SKIP=com/sun/tools/javac/jvm/ClassReader.readAttrs`
(this exact method alone: pass). Added `SkipReason::ClassReaderReadAttrs`
to `vm/src/jit/skip_list.rs`.

### Fix 2: `RootBeanDefinition.setResolvedFactoryMethod` native override wrote to a non-existent field

`processAheadOfTimeWithExplicitResolvableType` (gh-30689 -- a bean
definition built via `setResolvedFactoryMethod` + `setTargetType`, no
`factoryMethodName` ever set explicitly) failed with `IllegalStateException:
No constructor or factory method candidate found for ... factoryMethodName=
null`. Root-caused with a small standalone probe (`HBProbe.java`): calling
the native-overridden `setResolvedFactoryMethod` and immediately reading
back `getResolvedFactoryMethod()`/`getFactoryMethodName()` both returned
`null`. The override (`native-builtins/src/spring_startup_bootstrap.rs`,
originally landed for an unrelated `ModifiedClassPathClassLoader`/
loader-canonicalization fix) wrote the incoming `Method` to a field named
`"resolvedFactoryMethod"` -- confirmed against the current
`spring-framework-recheck` checkout's actual source that the real field is
`factoryMethodToIntrospect` -- so every write was silently absorbed by
`set_field_by_name`'s no-such-field path, and the override never replicated
`setResolvedFactoryMethod`'s real side effect of calling
`setUniqueFactoryMethodName(method.getName())` (setting both
`factoryMethodName` and `isFactoryMethodUnique = true`), which
`ConstructorResolver.resolveFactoryMethod` requires before it will even
consult `getResolvedFactoryMethod()`. Fixed by writing the correct field
name and replicating both side effects.

### Fix 3: CGLIB `FastClass` placeholder classes never emitted

`processAheadOfTimeWhenHasCglibProxyWriteProxyAndGenerateReflectionHints`'s
`isRegisteredCglibClass` helper checks THREE class names are present in
`TestGenerationContext.getGeneratedFiles()` with a matching reflection
hint: the main `$$SpringCGLIB$$0` enhancer (correctly emitted) plus TWO
`$$SpringCGLIB$$FastClass$$0`/`$$1` helper classes real cglib always emits
alongside it (a reflection-avoidance dispatch-table optimization). This
native `ConfigurationClassEnhancer.enhance()` reimplementation never
generates FastClass at all (`emit_bean_override`'s dispatch is inlined
directly, no indirection through a `Callback`/`FastClass` lookup table
needed), so the third `getGeneratedFileContent` assertion always saw
`null`, confirmed by a debug print added to a locally-patched copy of the
test (`isRegisteredCglibClass`) run against the real classpath jar via
front-of-classpath override -- main class: `content=present len=3620`;
`FastClass$$0`: `content=NULL`. Fixed with a NEW `build_fastclass_
placeholder` helper (`native-builtins/src/cglib_enhancer.rs`) emitting a
minimal `extends java/lang/Object` placeholder class (nothing ever loads
or invokes it) for each of the two names, fed through the SAME
`notify_generated_class_handler` hook the main class already uses -- real
Spring's `CglibClassHandler.handleGeneratedClass` generically registers
BOTH the `GeneratedFiles` entry and the `INVOKE_DECLARED_CONSTRUCTORS`
reflection hint for any name+bytes handed to it, so no separate hint-
registration code was needed on this side.

### Root-caused but NOT fixed: two CGLIB cross-test residuals, confirmed test-order-dependent

`processAheadOfTimeUsesCglibClassForFactoryMethod` ("`IllegalArgumentException:
class ... is not an enhanced class`") and `processAheadOfTimeWhenHasCglibProxyUseProxy`
("Hello1" instead of "Hello0" -- `CglibConfiguration.prefix()`'s body
running twice) were BOTH independently confirmed, via repeated isolated
`KRunMethod` runs, to **pass 100% reliably every time in isolation** but
**fail 100% deterministically** whenever run as part of the full
40-method `ApplicationContextAotGeneratorTests` class run (verified twice,
identical failure set both times -- not flaky/host-load-dependent).

Initial hypothesis: `config_enhancer_class_cache` (the cache added in an
earlier session so a repeat `enhance()` call for the SAME `@Configuration`
class returns the SAME `Class`, matching real CGLIB's own
`AbstractClassGenerator` caching) was keyed by the bare, **recyclable**
`ClassId` alone rather than `(defining_loader_id, class_name)` -- flagged
as a known gap in an earlier session's `ConfigurationClassEnhancerTests
.withPublicClass` note, and matching the sibling `config_enhancer_counters`
cache's own already-fixed key shape. **Fixed this** (now keyed by
`(loader_id, super_internal_name)`, mirroring `config_enhancer_counters`)
as a genuine, independent correctness improvement -- but empirically, via
a `CRATONVM_DBG_CCECACHE`-gated trace (kept in the code, see
`cce_enhance`), this did **not** turn out to be what's happening here:
both tests, run back-to-back in EITHER order via a minimal custom 2-method
JUnit launcher (`KRun2Methods.java`), showed the SECOND call reusing the
FIRST call's cache entry with the **exact same** `super_class_id` AND
`loader_id` both times -- i.e. `CglibConfiguration` genuinely is loaded
via the SAME stable loader across these nested test methods (contradicting
an initial assumption, checked with a minimal classloader-only repro, that
`@CompileWithForkedClassLoader` gives every test method method a fully
independent copy of every referenced class -- it does NOT for
`testFixtures` classes reached only by name, only for the outer test class
itself and anything the injected `classResourceLookup` covers). The cached
bytes themselves were independently confirmed correct (a `CBProbe.java`
probe directly enhancing `CglibConfiguration` and calling
`Enhancer.registerStaticCallbacks` on the result succeeded, setter method
present and reflectively found) -- so a cache HIT returning them should be
harmless. The actual mechanism was not further isolated this session:
attempts to trace deeper (running the two tests back-to-back with
`CRATONVM_DBG_CCECACHE=1`) repeatedly hit multi-minute delays around
Hibernate Validator's `ResourceBundleMessageInterpolator`/EL-processor
one-time initialization under host contention, consuming the remaining
investigation budget without a clean trace. **Next step for whoever
continues**: bypass the JUnit/Spring-context-refresh machinery entirely
(a raw Java program that directly exercises `ConfigurationClassPostProcessor`
+ `ApplicationContextAotGenerator.processAheadOfTime` twice in one process,
skipping anything that would trigger Hibernate Validator) to get a clean
multi-minute-hang-free trace of what differs between the cached-hit
`Class` mirror returned during AOT PROCESSING and whatever the REPLAY-time
compiled/loaded class actually is.

### New finding, not fixed: non-capturing lambdas aren't cached as JVM singletons

`processAheadOfTimeWhenHasAutowiringOnUnresolvedGeneric` (confirmed to
fail 100% reliably even in ISOLATION, not cross-test) asserts that
`AutowiredGenericTemplate.genericTemplate` (autowired in a FRESH, AOT-
replayed context) is `.equals()` (== identity, no custom `equals()`) to
`applicationContext.getBean("genericTemplate")` from the ORIGINAL context
used for AOT processing -- both ultimately backed by the exact same
`v -> {}` non-capturing lambda expression in `GenericTemplateConfiguration
.genericTemplate()`. On real HotSpot this holds because
`InnerClassLambdaMetafactory` special-cases a lambda with ZERO captured
arguments: it emits a single cached `private static final INSTANCE` field
on the spun-up hidden lambda class and every invocation of the factory
method just returns that same field, rather than allocating a fresh
instance -- a real, load-bearing JDK optimization, not just an incidental
detail. CratonVM's own lambda implementation (`vm/src/runtime/
invokedynamic.rs`'s `allocate_lambda_proxy`, called from both the fresh-
link and cached-call-site paths) unconditionally allocates a brand new
heap object on every invocation regardless of capture count, so two
separate invocations of the same non-capturing lambda expression produce
two non-identical (and non-`.equals()`) objects on this VM. **Not fixed
this session** -- implementing the singleton-per-zero-capture-callsite
cache correctly needs a GC-safe long-lived `ObjectRef` cache (precedent
exists: `native-builtins/src/lang_math.rs`'s `integer_cache` +
`gc_scan_value_of_cache_roots`/its remap counterpart for `Integer.valueOf`'s
`-128..127` boxed cache), keyed by something that survives class/loader
GC without recycling-related aliasing (the exact same recyclable-`ClassId`
concern as the CGLIB cache above) -- flagged as a real, understood, but
nontrivial VM-level gap for a dedicated follow-up, not attempted given
this session's remaining time budget.

### Not attempted this session (time budget)

Family B of `endToEndTestsForBeanOverrides` and the `TestContextAotGeneratorIntegrationTests`
`GroovySystem` quiet-host recheck (both already flagged above) were not
revisited this session either -- the host was never quiet, and this
session's remaining time went to the CGLIB cross-test investigation above
instead.

Verified: `cargo test -p cratonvm-native-builtins --lib` (3078 passed / 1
pre-existing unrelated failure -- `regex_lookbehind_tests::pem_block_to_der_roundtrip`,
crypto/PEM code untouched by this session -- / 6 ignored), `cargo test -p
cratonvm-vm --lib --release` both before merging `origin/dev` (2230
passed / 13 pre-existing `lock_order`/`skip_list`/`tomcat_scanner`
baseline failures) and after (2233 passed / 10 of the same family --
3 of the 13 were independently fixed by concurrent upstream work merged
in mid-session) -- zero regressions introduced by this session's 3 fixes
at any point. `ApplicationContextAotGeneratorTests` full class:
34/40 -> 36/40.

## 2026-07-24 AOT follow-up 8 -- three more residuals closed (34/40 -> 36/40), two CGLIB cross-test failures root-caused but not yet fixed, new lambda-singleton gap found

Worktree `/data/wt-aot-residuals3-20260723` (branch
`fix/aot-residuals3-20260723`, from `origin/dev` `1dae989b1`, merged with
`origin/dev` `d58e8ea1e` mid-session with no conflicts). No subagents used,
per this task's standing instruction. Pushed directly to `origin/dev` at
`ce0804f81` (fast-forward, `d58e8ea1e..ce0804f81`) — the shared main
checkout at `/data/data/cratonvm` had uncommitted changes belonging to
another concurrent session, so the merge-and-push was done entirely from
this session's own worktree instead of touching the shared checkout.

### Fix 1: `ClassReader.readAttrs` -- SEVENTH JIT-miscompile residual in the repeated-in-process-javac family

Same family as `ClassReaderReadClass`/`ClassFinderComplete`/
`ClassFinderFillIn`/`ClassReaderReadInnerClasses`/`ClassSymbolComplete`
(see follow-up 7 and earlier entries above). New symptom: `bad class
file... bad signature: "ourceFile"` (the leading `S` of `SourceFile` lost
-- a `ClassReader.bp` cursor desync) while compiling AOT-generated sources
against classpath `.class` files, reproduced deterministically via
`ApplicationContextAotGeneratorTests$ConfigurationClassCglibProxy
.processAheadOfTimeWhenHasCglibProxyUseProxy` even with every previously-
known culprit already interpreted. Root-caused to `ClassReader.readAttrs`
-- the attribute-dispatch loop `readClassAttrs`/`readMemberAttrs` both
delegate straight into (read a count via `nextChar()`, then loop reading a
name-index `nextChar()` + length `nextInt()` per entry) -- via the
established methodology: `--nojit` control (pass), `CRATONVM_JIT_DENY=
com/sun/tools/javac/jvm/ClassReader` (whole class: pass), then
`CRATONVM_JIT_BISECT_SKIP=com/sun/tools/javac/jvm/ClassReader.readAttrs`
(this exact method alone: pass). Added `SkipReason::ClassReaderReadAttrs`
to `vm/src/jit/skip_list.rs`.

### Fix 2: `RootBeanDefinition.setResolvedFactoryMethod` native override wrote to a non-existent field

`processAheadOfTimeWithExplicitResolvableType` (gh-30689 -- a bean
definition built via `setResolvedFactoryMethod` + `setTargetType`, no
`factoryMethodName` ever set explicitly) failed with `IllegalStateException:
No constructor or factory method candidate found for ... factoryMethodName=
null`. Root-caused with a small standalone probe (`HBProbe.java`): calling
the native-overridden `setResolvedFactoryMethod` and immediately reading
back `getResolvedFactoryMethod()`/`getFactoryMethodName()` both returned
`null`. The override (`native-builtins/src/spring_startup_bootstrap.rs`,
originally landed for an unrelated `ModifiedClassPathClassLoader`/
loader-canonicalization fix) wrote the incoming `Method` to a field named
`"resolvedFactoryMethod"` -- confirmed against the current
`spring-framework-recheck` checkout's actual source that the real field is
`factoryMethodToIntrospect` -- so every write was silently absorbed by
`set_field_by_name`'s no-such-field path, and the override never replicated
`setResolvedFactoryMethod`'s real side effect of calling
`setUniqueFactoryMethodName(method.getName())` (setting both
`factoryMethodName` and `isFactoryMethodUnique = true`), which
`ConstructorResolver.resolveFactoryMethod` requires before it will even
consult `getResolvedFactoryMethod()`. Fixed by writing the correct field
name and replicating both side effects.

### Fix 3: CGLIB `FastClass` placeholder classes never emitted

`processAheadOfTimeWhenHasCglibProxyWriteProxyAndGenerateReflectionHints`'s
`isRegisteredCglibClass` helper checks THREE class names are present in
`TestGenerationContext.getGeneratedFiles()` with a matching reflection
hint: the main `$$SpringCGLIB$$0` enhancer (correctly emitted) plus TWO
`$$SpringCGLIB$$FastClass$$0`/`$$1` helper classes real cglib always emits
alongside it (a reflection-avoidance dispatch-table optimization). This
native `ConfigurationClassEnhancer.enhance()` reimplementation never
generates FastClass at all (`emit_bean_override`'s dispatch is inlined
directly, no indirection through a `Callback`/`FastClass` lookup table
needed), so the third `getGeneratedFileContent` assertion always saw
`null`, confirmed by a debug print added to a locally-patched copy of the
test (`isRegisteredCglibClass`) run against the real classpath jar via
front-of-classpath override -- main class: `content=present len=3620`;
`FastClass$$0`: `content=NULL`. Fixed with a NEW `build_fastclass_
placeholder` helper (`native-builtins/src/cglib_enhancer.rs`) emitting a
minimal `extends java/lang/Object` placeholder class (nothing ever loads
or invokes it) for each of the two names, fed through the SAME
`notify_generated_class_handler` hook the main class already uses -- real
Spring's `CglibClassHandler.handleGeneratedClass` generically registers
BOTH the `GeneratedFiles` entry and the `INVOKE_DECLARED_CONSTRUCTORS`
reflection hint for any name+bytes handed to it, so no separate hint-
registration code was needed on this side.

### Root-caused but NOT fixed: two CGLIB cross-test residuals, confirmed test-order-dependent

`processAheadOfTimeUsesCglibClassForFactoryMethod` ("`IllegalArgumentException:
class ... is not an enhanced class`") and `processAheadOfTimeWhenHasCglibProxyUseProxy`
("Hello1" instead of "Hello0" -- `CglibConfiguration.prefix()`'s body
running twice) were BOTH independently confirmed, via repeated isolated
`KRunMethod` runs, to **pass 100% reliably every time in isolation** but
**fail 100% deterministically** whenever run as part of the full
40-method `ApplicationContextAotGeneratorTests` class run (verified twice,
identical failure set both times -- not flaky/host-load-dependent).

Initial hypothesis: `config_enhancer_class_cache` (the cache added in an
earlier session so a repeat `enhance()` call for the SAME `@Configuration`
class returns the SAME `Class`, matching real CGLIB's own
`AbstractClassGenerator` caching) was keyed by the bare, **recyclable**
`ClassId` alone rather than `(defining_loader_id, class_name)` -- flagged
as a known gap in an earlier session's `ConfigurationClassEnhancerTests
.withPublicClass` note, and matching the sibling `config_enhancer_counters`
cache's own already-fixed key shape. **Fixed this** (now keyed by
`(loader_id, super_internal_name)`, mirroring `config_enhancer_counters`)
as a genuine, independent correctness improvement -- but empirically, via
a `CRATONVM_DBG_CCECACHE`-gated trace (kept in the code, see
`cce_enhance`), this did **not** turn out to be what's happening here:
both tests, run back-to-back in EITHER order via a minimal custom 2-method
JUnit launcher (`KRun2Methods.java`), showed the SECOND call reusing the
FIRST call's cache entry with the **exact same** `super_class_id` AND
`loader_id` both times -- i.e. `CglibConfiguration` genuinely is loaded
via the SAME stable loader across these nested test methods (contradicting
an initial assumption, checked with a minimal classloader-only repro, that
`@CompileWithForkedClassLoader` gives every test method method a fully
independent copy of every referenced class -- it does NOT for
`testFixtures` classes reached only by name, only for the outer test class
itself and anything the injected `classResourceLookup` covers). The cached
bytes themselves were independently confirmed correct (a `CBProbe.java`
probe directly enhancing `CglibConfiguration` and calling
`Enhancer.registerStaticCallbacks` on the result succeeded, setter method
present and reflectively found) -- so a cache HIT returning them should be
harmless. The actual mechanism was not further isolated this session:
attempts to trace deeper (running the two tests back-to-back with
`CRATONVM_DBG_CCECACHE=1`) repeatedly hit multi-minute delays around
Hibernate Validator's `ResourceBundleMessageInterpolator`/EL-processor
one-time initialization under host contention, consuming the remaining
investigation budget without a clean trace. **Next step for whoever
continues**: bypass the JUnit/Spring-context-refresh machinery entirely
(a raw Java program that directly exercises `ConfigurationClassPostProcessor`
+ `ApplicationContextAotGenerator.processAheadOfTime` twice in one process,
skipping anything that would trigger Hibernate Validator) to get a clean
multi-minute-hang-free trace of what differs between the cached-hit
`Class` mirror returned during AOT PROCESSING and whatever the REPLAY-time
compiled/loaded class actually is.

### New finding, not fixed: non-capturing lambdas aren't cached as JVM singletons

`processAheadOfTimeWhenHasAutowiringOnUnresolvedGeneric` (confirmed to
fail 100% reliably even in ISOLATION, not cross-test) asserts that
`AutowiredGenericTemplate.genericTemplate` (autowired in a FRESH, AOT-
replayed context) is `.equals()` (== identity, no custom `equals()`) to
`applicationContext.getBean("genericTemplate")` from the ORIGINAL context
used for AOT processing -- both ultimately backed by the exact same
`v -> {}` non-capturing lambda expression in `GenericTemplateConfiguration
.genericTemplate()`. On real HotSpot this holds because
`InnerClassLambdaMetafactory` special-cases a lambda with ZERO captured
arguments: it emits a single cached `private static final INSTANCE` field
on the spun-up hidden lambda class and every invocation of the factory
method just returns that same field, rather than allocating a fresh
instance -- a real, load-bearing JDK optimization, not just an incidental
detail. CratonVM's own lambda implementation (`vm/src/runtime/
invokedynamic.rs`'s `allocate_lambda_proxy`, called from both the fresh-
link and cached-call-site paths) unconditionally allocates a brand new
heap object on every invocation regardless of capture count, so two
separate invocations of the same non-capturing lambda expression produce
two non-identical (and non-`.equals()`) objects on this VM. **Not fixed
this session** -- implementing the singleton-per-zero-capture-callsite
cache correctly needs a GC-safe long-lived `ObjectRef` cache (precedent
exists: `native-builtins/src/lang_math.rs`'s `integer_cache` +
`gc_scan_value_of_cache_roots`/its remap counterpart for `Integer.valueOf`'s
`-128..127` boxed cache), keyed by something that survives class/loader
GC without recycling-related aliasing (the exact same recyclable-`ClassId`
concern as the CGLIB cache above) -- flagged as a real, understood, but
nontrivial VM-level gap for a dedicated follow-up, not attempted given
this session's remaining time budget.

### Not attempted this session (time budget)

Family B of `endToEndTestsForBeanOverrides` and the `TestContextAotGeneratorIntegrationTests`
`GroovySystem` quiet-host recheck (both already flagged above) were not
revisited this session either -- the host was never quiet, and this
session's remaining time went to the CGLIB cross-test investigation above
instead.

Verified: `cargo test -p cratonvm-native-builtins --lib` (3078 passed / 1
pre-existing unrelated failure -- `regex_lookbehind_tests::pem_block_to_der_roundtrip`,
crypto/PEM code untouched by this session -- / 6 ignored), `cargo test -p
cratonvm-vm --lib --release` both before merging `origin/dev` (2230
passed / 13 pre-existing `lock_order`/`skip_list`/`tomcat_scanner`
baseline failures) and after (2233 passed / 10 of the same family --
3 of the 13 were independently fixed by concurrent upstream work merged
in mid-session) -- zero regressions introduced by this session's 3 fixes
at any point. `ApplicationContextAotGeneratorTests` full class:
34/40 -> 36/40.

## 2026-07-24 AOT follow-up 9 -- non-capturing lambda singleton cache fixed (36/40 -> 38/40), CGLIB cross-test residual narrowed but not fixed

Worktree `/data/wt-aot-followup9-20260724` (branch `fix/aot-followup9-20260724`,
from `origin/dev` `93ec6a810`). No subagents used, per this task's standing
instruction.

### Fix: non-capturing lambdas now cache a per-call-site singleton

Implemented the gap flagged (not fixed) at the end of follow-up 8: real
HotSpot's `InnerClassLambdaMetafactory` caches a single `INSTANCE` per spun
lambda class when the lambda captures zero arguments, and some code (this
AOT test) depends on that identity holding across two separate invocations
of the same non-capturing lambda expression. Added a
`LAMBDA_SINGLETON_CACHE` (`vm/src/runtime/invokedynamic.rs`, keyed by
`(vm_identity, proxy_class_id)`) that `allocate_lambda_proxy` consults
before allocating: a zero-capture call site mints its singleton exactly
once and every later invocation hits the cache instead of allocating a
fresh proxy object. `proxy_class_id` (`SharedVm::alloc_lambda_proxy_id`) is
a monotonically-increasing id that is **never recycled** (confirmed by
reading the allocator: plain `AtomicU32::fetch_add`, no reuse), unlike the
real, recyclable `ClassId` space used for loaded classes -- so unlike the
CGLIB cache below, this key carries no aliasing risk and needed no extra
care. GC-scanned/remapped the same way as the `Integer.valueOf` cache in
`native-builtins/src/lang_math.rs` (new `gc_scan_lambda_singleton_roots`/
`gc_update_lambda_singleton_refs`, wired into `vm/src/memory/{roots.rs,gc.rs}`
as step 16a, right after the existing step-16 LambdaMetafactory CallSite
cache).

Fixes `processAheadOfTimeWhenHasAutowiringOnUnresolvedGeneric`. Verified via
isolated `MethodRun` and via the full `ApplicationContextAotGeneratorTests`
class: 36/40 -> 38/40, only the two already-known CGLIB cross-test
residuals remain (`processAheadOfTimeUsesCglibClassForFactoryMethod`,
`processAheadOfTimeWhenHasCglibProxyUseProxy`).

### CGLIB cross-test residual: two more hypotheses ruled out, real mechanism narrowed

Continued follow-up 8's "next step" (bypass JUnit/Spring-context-refresh
machinery for a clean repro). Two hypotheses that looked highly plausible
going in were both **refuted** with direct evidence:

1. **Loader-id recycling.** Checked `ClassManager`'s own doc comment
   (`classloading/src/class_manager.rs` around `user_loaders`): "the set
   only grows because class-loader unloading is not implemented in this
   VM." Confirmed at the allocator itself
   (`vm/src/vm/vm_exec.rs::allocate_loader_id`): a plain
   `AtomicU32::fetch_add`, starting at 3, never recycled. Two different
   `new CompileWithForkedClassLoaderClassLoader(...)` instances (Spring's
   real per-`@Test`-method forking mechanism, `spring-core-test.jar`)
   genuinely get distinct, monotonically-increasing loader ids -- ruled
   out as the cause.
2. **The forked-classloader mechanism itself being loader-blind for
   testfixture classes reached only by name** (follow-up 8's tentative
   read of its own `CRATONVM_DBG_CCECACHE` trace). Built two
   Spring-free/CGLIB-free repros using Spring's REAL
   `CompileWithForkedClassLoaderClassLoader`/`CompileWithForkedClassLoader`
   classes (already on the classpath, `org.springframework.core.test.tools`)
   against a plain fixture class with a static `AtomicInteger` counter --
   one flat (`ProbeForkTest`, two top-level `@Test` methods), one nested
   with the counter-touching call routed through a private helper method
   on the OUTER class (`OuterProbeTest.Nested2`, matching
   `ApplicationContextAotGeneratorTests.ConfigurationClassCglibProxy`'s own
   shape exactly). **Both reproduced correctly on CratonVM**: fresh counter
   (`==1`) and a fresh, distinct defining loader on every forked
   invocation, matching real HotSpot's behavior byte-for-byte. This rules
   out "the generic forking mechanism is broken" as the cause -- whatever
   is happening is specific to the CGLIB/`ConfigurationClassPostProcessor`
   path, not to `@CompileWithForkedClassLoader` in general.

**New finding, narrows the real mechanism**: re-ran the full 40-method
class with `CRATONVM_DBG_CCECACHE=1`. Across the whole run,
`config_enhancer_class_cache`'s lookup key
(`(super_loader_id, super_class_name)`) is correctly **unique per test
method** -- 9 distinct loader ids observed for 9 CGLIB-using test methods,
confirming (again) no loader-id aliasing across tests. But **within the
two failing tests' own single loader scope**, `ConfigurationClassEnhancer
.enhance()` is called **twice** for the identical `(loader_id,
"CglibConfiguration")` key, and **both calls report a cache MISS** --
which is itself the anomaly: a same-loader repeat call should hit the
cache the second time (real cglib's own `AbstractClassGenerator` caching
guarantees the same generated `Class` on a repeat `enhance()` call for the
same superclass+loader, which is exactly what `config_enhancer_class_cache`
was added to emulate). A double-miss means two DIFFERENT `ClassId`s get
minted for what should be "the same" enhancer class within one test's own
scope -- if the AOT-generated source (compiled once, referencing
whichever `ClassId` was live when generation ran) and the actual
CGLIB-marker/dispatch check at replay time (whichever `ClassId` a second,
missed-cache `enhance()` call produced) end up disagreeing about which of
the two is canonical, that would explain both symptoms directly: "not an
enhanced class" (an identity/marker check against the wrong `ClassId`) and
"Hello1" instead of "Hello0" (the underlying real `CglibConfiguration`'s
static `AtomicInteger` counter genuinely getting exercised twice within
one test run, once per enhance() attempt, if scanning/building a second
enhancer subclass also touches the superclass's own state).

Two tests with a bare `CglibConfiguration` (not one of the `Value/
Autowired/Configurable` variants) are exactly `processAheadOfTime
UsesCglibClassForFactoryMethod` and `processAheadOfTimeWhenHasCglibProxy
UseProxy` -- i.e. this double-miss-within-one-test pattern maps 1:1 onto
the two actually-failing tests, and does NOT occur for any of the other
CGLIB-using tests in the class (which call `enhance()` either once, or
twice with the second call correctly hitting the cache). **Not yet
explained**: why `testCompiledResult`'s generation+replay flow calls
`enhance()` twice specifically for these two tests and not the others
(`processAheadOfTimeWhenHasCglibProxyAndAutowiring`/`AndMixedAutowiring`/
`WithArgumentsUseProxy`, all similarly `testCompiledResult`-based, showed
a clean single miss + single hit in the same trace), nor why the SECOND
call misses instead of hitting given an apparently-identical cache key.
**Next step for whoever continues**: instrument `cce_enhance` with a
cache-size print immediately before/after both the lookup and the insert
(not just hit/miss) to rule out reentrancy (the second call happening
before the first's insert completes, e.g. via a recursive trigger during
`scan_bean_methods`/class initialization) versus the key itself somehow
differing between the two calls despite printing identically in the
existing trace; a raw (non-JUnit, non-Spring-context) driver that calls
`ConfigurationClassPostProcessor`/`ApplicationContextAotGenerator
.processAheadOfTime` directly, twice, in one process for
ONLY these two specific config classes would isolate this far faster than
the ~5-minute full-class run this session used.

### Also found, out of scope, flagged separately

`cargo test -p cratonvm-native-builtins --lib` surfaced a NEW failure not
present in follow-up 8's baseline:
`cglib_enhancer::fb_ref_bytecode_tests::fb_ref_splice_shifts_exception_table_by_exactly_8_bytes`
(`left: 281 right: 161` at `cglib_enhancer.rs:4668`, deterministic in
isolation). Confirmed pre-existing on `origin/dev` (this session never
touched `cglib_enhancer.rs`) -- most likely `86b638d65`
("parameterized @Bean methods" / `@Lookup` fixes) changed
`emit_bean_override`'s generated bytecode shape without updating this
test's hardcoded byte-length constants. Flagged via a spawned background
task rather than fixed here to stay in scope.

### Not attempted this session (time budget)

Family B of `endToEndTestsForBeanOverrides` and the
`TestContextAotGeneratorIntegrationTests` `GroovySystem` quiet-host
recheck (both flagged since follow-up 7/8) were not revisited this session
either -- the CGLIB cross-test investigation above consumed the bulk of
this session's time budget.

Verified: `cargo test -p cratonvm-vm --lib --release` (2233 passed / 10
pre-existing `lock_order`/`tomcat_scanner` baseline failures -- exact same
family/count as follow-up 8's post-merge baseline, zero regressions from
this session's lambda-cache fix). `cargo test -p cratonvm-native-builtins
--lib` (3077 passed / 2 failed -- the 1 pre-existing
`pem_block_to_der_roundtrip` plus the newly-surfaced, pre-existing-on-dev
`fb_ref_splice_shifts_exception_table_by_exactly_8_bytes` above, neither
caused by this session). `ApplicationContextAotGeneratorTests` full class:
36/40 -> 38/40.

## 2026-07-24 AOT follow-up 9b -- Family B investigation blocked by two newly-found regressions (neither fixed), both characterized with repros

Continuation of follow-up 9's session (same worktree, `/data/wt-aot-followup9-20260724`).
Attempted to isolate "Family B" of `endToEndTestsForBeanOverrides` (the
plain `AssertionFailedError: expected: null but was: ""` failures flagged
since [[aot-endtoend-beanoverrides-73-to-158-of-175]] and never isolated to
specific test classes). Did not reach it -- hit two separate, apparently-NEW
blocking issues in sequence, neither present when the prior session
(2026-07-23) got a clean ~158/175 baseline on the exact same test method.
Both are real VM bugs, fully characterized with standalone repro commands,
but not fixed this session (time budget).

### Blocker 1: `Package.getPackageInfo()` NPE during TestNG-engine classpath discovery

Running `AotIntegrationTests` (whole class, or just `endToEndTestsForBeanOverrides`
via `MethodRun`) now fails immediately (~22s) with:

```
org.junit.platform.commons.JUnitException: TestEngine with ID 'testng' failed to discover tests
Caused by: java.lang.NullPointerException: Cannot invoke "java.lang.Module.getClassLoader()" because "module" is null
    java.lang.Package.getPackageInfo(Package.java:417)
    java.lang.Package.getAnnotation(Package.java:446)
    org.testng.internal.annotations.IgnoreListener.findAnnotation(...)
    ...
    org.springframework.test.context.aot.TestClassScanner.scan(TestClassScanner.java:156)
```

`TestClassScanner.scan()` (spring-core-test) uses a plain `LauncherFactory
.create()` (all registered engines auto-discovered via ServiceLoader,
including `org.junit.support.testng`'s TestNG-compat engine) with
`selectClasspathRoots(...)` -- meaning the TestNG engine's own
`TestNGClassFinder` walks (a large slice of) the classpath root looking for
TestNG-style classes, and NPEs on the FIRST class whose `Class.getPackage()`
returns a `Package` object with a null `module` field (real JDK requires
the `module` field to always be at least the defining loader's unnamed
module, never null).

**Isolation attempts, both inconclusive** (2 standalone, Spring-free
repros): a plain `-cp`-loaded class's `getPackage()` correctly returns a
non-null module on CratonVM (though the module's OWN `.getClassLoader()`
already differs from real JDK -- see below); adding a `package-info.java`
with a runtime-retained annotation to force the real `getPackageInfo()`
path (`Class.forName(module, pkg + ".package-info")`) still worked fine.
**Neither reproduces the NPE** -- whatever class/loader combination TestNG's
crawler hits during Spring's classpath-root scan produces a `Package` with
a genuinely null `module` field, and it wasn't isolated to a specific class
this session (the TestNG engine's own class-by-class walk order wasn't
traced). A secondary, likely-related but non-crashing gap found along the
way: `Class.getModule().getClassLoader()` returns `null` on CratonVM for an
ordinary `-cp`-loaded class's unnamed module, where real JDK returns the
actual `AppClassLoader` -- this alone doesn't crash anything found this
session, but is almost certainly the same underlying gap (module objects
not fully wired to their defining loader) just not always fatal.

**Workaround used to unblock further investigation** (not a fix): strip
`testng-engine-*.jar` and `testng-*.jar` from the classpath entirely before
invoking `AotIntegrationTests` --
`cat cratonvm-testcp.txt | tr ':' '\n' | grep -v -i testng | tr '\n' ':'`
-- since `TestClassScanner` only needs the JUnit Jupiter engine for Spring's
own AOT integration tests; removing the optional TestNG engine sidesteps
the crash entirely and lets discovery proceed.

**Next step for whoever continues**: instrument `TestClassScanner.scan()`
(or run with a debug agent) to print the exact class name being examined
when the NPE fires, then trace how THAT class's `Package` object got built
with a null `module` -- likely in `vm/src/native` wherever `Class
.getPackage()`/`ClassLoader.definePackage`-equivalent synthetic Package
construction happens, check whether it's conditioned on the loader type
(bootstrap/app/user-defined) and misses a case.

### Blocker 2: `Class.getDeclaredMethods()` NoSuchMethodError inside a dynamically-defined (non-file) CGLIB class's `<clinit>` -- confirmed a NEW regression

With Blocker 1 worked around, `endToEndTestsForBeanOverrides` actually ran
(~70s) and reached real AOT PROCESSING -- but `runEndToEndTests(testClasses,
true)` uses `failOnError=true`, so it stops at the FIRST test class whose
AOT generation fails, rather than aggregating all 175 like the final
(replay-phase) failure list follow-up 7/8's sessions saw. First (alphabetical
discovery order) failure:

```
TestContextAotException: Failed to generate AOT artifacts for test classes
  [...MockitoSpyBeanAndCircularDependenciesWithLazyResolutionProxyIntegrationTests]
Caused by: AotBeanProcessingException: Error processing bean ...$One
Caused by: AopConfigException: Unexpected AOP exception
Caused by: IllegalStateException: Unable to load cache item
  org.springframework.cglib.core.internal.LoadingCache.createEntry
  org.springframework.cglib.core.AbstractClassGenerator.create
  org.springframework.aop.framework.ObjenesisCglibAopProxy.createProxyClass
Caused by: java.lang.NoSuchMethodError: java.lang.Class.getDeclaredMethods()[Ljava/lang/reflect/Method;
  ...MockitoSpyBeanAndCircularDependenciesWithLazyResolutionProxyIntegrationTests$Two$$SpringCGLIB$$0.CGLIB$STATICHOOK1(<generated>)
  ...MockitoSpyBeanAndCircularDependenciesWithLazyResolutionProxyIntegrationTests$Two$$SpringCGLIB$$0.<clinit>(<generated>)
  org.springframework.cglib.core.ReflectUtils.defineClass(ReflectUtils.java:581)
```

Important: this is **real, bytecode-generated cglib** (`org.springframework
.cglib.core.ReflectUtils.defineClass` / `Enhancer.generate()`, Spring's
repackaged-cglib for `CglibAopProxy` AOP proxies via `ContextAnnotation
AutowireCandidateResolver.buildLazyResolutionProxy` -- a "lazy resolution
proxy" for a circular-dependency `@Autowired` field), NOT the native
`cce_enhance`/`ConfigurationClassEnhancer.enhance()` override this whole
CGLIB-cluster investigation (follow-up 8/9) has otherwise been chasing --
i.e. this is a THIRD, distinct CGLIB code path in CratonVM (native
`ConfigurationClassEnhancer` override, native `FastClass` placeholder, and
now real-bytecode-executed `cglib.core`/`Enhancer` proxy generation all
exist independently). `CGLIB$STATICHOOK1` is cglib's universal
per-generated-class static initializer that populates its internal
method-interception tables by reflectively calling `Class
.getDeclaredMethods()` on itself -- a completely standard, ubiquitous cglib
pattern, so this is NOT specific to lazy-resolution proxies; any real-cglib
(non-Spring-Configuration) proxy generation likely hits the same wall.

`Class.getDeclaredMethods()` IS registered as an unconditional native
override for `java/lang/Class` (`native-builtins/src/lib.rs`, delegates to
`lang_class::native_class_get_declared_methods`) -- the `NoSuchMethodError`
is therefore NOT a missing-registration gap but a method-RESOLUTION failure
specific to this receiver, most likely something about how CratonVM
resolves a `Methodref` constant-pool entry against `java/lang/Class` from
WITHIN a dynamically-`defineClass`'d (not loaded from a `.class` file on
disk) class's own bytecode -- not investigated further this session.

**Confirmed to be a genuine regression, not a pre-existing gap**: follow-up
7's own memory note ([[aot-endtoend-beanoverrides-73-to-158-of-175]])
recorded a clean **158/175** pass on this EXACT test method
(`endToEndTestsForBeanOverrides`, same `failOnError=true`) on 2026-07-23 --
if this CGLIB-proxy class already failed AOT processing then, `failOnError
=true` would have aborted immediately with 1/175, not proceeded to
aggregate 17 replay-phase failures. Something merged into `origin/dev`
between that session and this one (31+ commits of drift on top of this
session's own base) broke real-bytecode cglib class generation's
`Class.getDeclaredMethods()` resolution. Bisecting the responsible commit
was not attempted this session.

**Next step for whoever continues**: (1) bisect `origin/dev` between the
2026-07-23 session's commit and now for whatever changed
`Class.getDeclaredMethods` resolution, dynamic-`defineClass` constant-pool
resolution, or cglib-adjacent native code -- likely a much smaller, faster
fix than re-deriving root cause from scratch; (2) once found/fixed, rerun
`endToEndTestsForBeanOverrides` (classpath with `testng` jars stripped per
Blocker 1's workaround, `--Xmx 4g`, expect several more classes to hit
similar or different failures before Family B's null-vs-"" symptom
actually surfaces -- budget for multiple iterations); (3) Blocker 1 (the
Package/Module NPE) should also be fixed independently since it silently
breaks ANY test suite that includes the TestNG JUnit-Platform engine on the
classpath, a much broader blast radius than just this one Spring test.

Family B itself remains completely unisolated -- zero net progress on the
original goal this session, but two real, previously-unknown-to-this-suite
bugs found and characterized instead.

## 2026-07-24 AOT follow-up 9c -- TestContextAotGeneratorIntegrationTests "GroovySystem hang" REFUTED as Groovy-related AND as JIT-related; real hang site narrowed to Spliterators.spliterator()/ArraySpliterator construction; one real (but insufficient) bug fixed along the way

Re-ran `TestContextAotGeneratorIntegrationTests` on a genuinely quiet host
(`uptime` load average ~0.4-1.6, the condition
[[testcontextaotgeneratorintegrationtests-groovysystem-clinit-hang]] said
was needed to distinguish a real hang from host-contention artifact),
`--stack-dump-on-timeout 600`. **Both of that memory's live hypotheses are
now refuted.**

### Refuted: "genuine (if extreme) Groovy MetaClassRegistryImpl bootstrap slowness"

CratonVM's own T19.H1 native-hang watchdog fired at the 600s mark and
self-aborted with a full diagnostic dump. No `GroovySystem`/Groovy anywhere
in the dump. The watchdog's thread summary and per-thread **native-call
ring buffer** (64 entries, oldest first) instead show the main thread
`STILL-IN-NATIVE(550000ms+ ago)` inside `java/util/Arrays.stream(
[Ljava/lang/Object;)Ljava/util/stream/Stream;`, reached from
`org/springframework/aot/generate/AccessControl.lowest([...])` (real
source: `Arrays.stream(candidates).map(AccessControl::getVisibility)
.toArray(Visibility[]::new)`), itself reached via `DefaultListableBeanFactory
.findAllAnnotationsOnBean` <- `RuntimeHintsBeanFactoryInitializationAotProcessor
.extractFromBeanFactory/processAheadOfTime`. Reproduced identically twice
(both attempts hung at the exact same site, ruling out a one-off fluke).

### Investigated and FIXED (but did not resolve the hang): missing `fence` field on synthetic Spliterators

`Arrays.stream(T[])`'s native override (`native-builtins/src/lib.rs`)
delegates to `Arrays.asList(arr).stream()`, which resolves through
`Collection.stream()`'s default method -> `spliterator()`. The native
`p59_collection_spliterator`/`p59_hashset_spliterator` implementations
(`native-builtins/src/phases_late.rs`) build a synthetic `java/util/
Spliterator` object but -- despite their own doc comment stating the type
is a "3-field (elements=0 Object[], pos=1 Int, fence=2 Int)" layout --
only allocated 2 fields and never wrote field 2 (`fence`) at all, while
`tryAdvance`/`estimateSize`/`characteristics`/`forEachRemaining` all read
field 2 as the exclusive upper bound. **Fixed**: both functions now
allocate 3 fields and set `fence` to the backing array/key-list's real
length. This is a genuine, real correctness bug (confirmed via code
reading, not just inference) and is kept regardless of the outcome below --
verified zero regressions (`cargo test -p cratonvm-vm --lib --release`
2233 passed/10 pre-existing-family failures matching baseline exactly
after re-running the one flaky new failure in isolation --
`native::jni::tests::process_vm_publish_and_resolve` passed cleanly alone,
confirming it was parallel-execution flakiness, not caused by this fix;
`cargo test -p cratonvm-native-builtins --lib` 3076 passed/4 failed, same
known family as before this session's other work, zero new failures).

**However: re-running the exact same hang scenario with the fix applied
reproduces the IDENTICAL hang, same site, same ~550s.** The fence-field
bug, while real, is not (solely) responsible for this hang.

### Refuted: JIT miscompilation

Given this codebase's extensive precedent of JIT-miscompile bugs in
adjacent areas (see `vm/src/jit/skip_list.rs`'s `ClassReaderReadClass`
family), re-ran with `--nojit` (interpreter-only). **The hang reproduces
identically** -- same site conceptually, though the EXACT call sequence at
the point of freezing differs slightly between JIT and no-JIT runs (see
below), ruling out a JIT-compiled-code-specific miscompilation as the
cause. This is a real interpreter/native-dispatch bug, not a codegen bug.

### Narrowed further: the no-JIT run reveals a DIFFERENT concrete call site than initially assumed

The `--nojit` run's dispatch trace, at the point of freezing, shows:

```
[...] BC  java/util/Collection.stream()Ljava/util/stream/Stream;
[...] NAT java/util/Spliterators.spliterator([Ljava/lang/Object;I)Ljava/util/Spliterator;  ->NATIVE java/util/Objects.requireNonNull(Ljava/lang/Object;)Ljava/lang/Object;
===== end dispatch_trace dump =====
```

This is a DIFFERENT path than `p59_collection_spliterator` (which this
session's fix targeted): `java.util.Spliterators.spliterator(Object[],
int)` is the real JDK STATIC factory method that `java.util.Arrays
$ArrayList.spliterator()` (the actual concrete class `Arrays.asList()`
returns on a real JDK -- a private nested class distinct from `java.util
.ArrayList`) calls, per its real source:
`return Spliterators.spliterator(a, Spliterator.ORDERED);`. Its own real
source is `return new Spliterators.ArraySpliterator<>(Objects
.requireNonNull(array), additionalCharacteristics);` -- i.e. it calls
`Objects.requireNonNull` (confirmed via code reading to be a trivial,
non-blocking native: `native_objects_require_non_null` in
`native-builtins/src/lib.rs`, cannot itself hang) and then constructs a
`java.util.Spliterators$ArraySpliterator` -- a REAL, bytecode-defined JDK
class, NOT one of CratonVM's synthetic objects.

**This means the actual hang is most likely inside either (a) object
allocation/constructor execution for `Spliterators$ArraySpliterator`
itself, or (b) whatever real bytecode runs immediately after
`Collection.stream()` returns this real Spliterator to `StreamSupport
.stream()` and onward through the `.map()`/`.toArray()` pipeline stages
`AccessControl.lowest` chains -- NOT inside any of the synthetic-object
native overrides this session inspected.** Given `Arrays.asList()`'s
native override (registered `"java/util/Arrays"`/`"asList"`) stamps its
returned synthetic object as plain `"java/util/ArrayList"` rather than
`"java/util/Arrays$ArrayList"`, there is likely a genuine class-identity
mismatch between what CratonVM's native `Arrays.asList` returns and what
real JDK bytecode (`Arrays$ArrayList.spliterator()`, only reachable if the
object's class resolves correctly to `Arrays$ArrayList`) expects to run --
worth checking whether method resolution for `spliterator()`/`stream()` on
this synthetic object is landing on the RIGHT declaring class consistently
across JIT vs no-JIT execution, since the two runs took visibly different
call paths to reach conceptually the same operation.

### Next step for whoever continues

1. Do NOT assume the fence-field fix (already landed) is sufficient --
   verify against a rebuild.
2. Attach a live debugger per the watchdog's own on-screen instructions
   (`gdb -p <pid>` within the 3s grace window after the dump; lower
   `--stack-dump-on-timeout` to trigger sooner for faster iteration) to
   get a REAL native backtrace of the frozen call -- this is now the only
   way to make further progress; static code reading has been pushed as
   far as it reasonably can without seeing the actual stuck frame.
3. Consider whether `Arrays.asList()`'s native override should stamp its
   result as `java/util/Arrays$ArrayList` (loading/using the REAL JDK
   class, if CratonVM's real-JDK mode can do so) rather than the
   synthetic `java/util/ArrayList`, to make `spliterator()`/`stream()`
   resolution match real JDK's actual dispatch (real `ArraySpliterator`
   construction) instead of landing inconsistently between a synthetic
   native override (JIT run) and real bytecode (no-JIT run) depending on
   execution mode -- this divergence is itself suspicious and worth
   investigating even independent of the hang.
4. A minimal standalone repro of the same `Arrays.stream(arr).map(...)
   .toArray(...)` shape (this session tried `String[3]` with `.map()`
   +`.toArray()`) does NOT reproduce in isolation, meaning some
   additional state (heap layout, prior GC activity, or receiver-class
   identity established only after thousands of prior class loads) is
   needed to trigger it -- a repro embedded inside (or immediately after)
   a real `RuntimeHintsBeanFactoryInitializationAotProcessor
   .extractFromBeanFactory` run, rather than a fresh-process synthetic
   test, is more likely to reproduce reliably.

This memory should be considered **superseded**:
[[testcontextaotgeneratorintegrationtests-groovysystem-clinit-hang]]'s
"most likely genuine (if extreme) Groovy bootstrap slowness" conclusion no
longer holds -- this session found zero Groovy involvement, ruled out JIT
miscompilation, and identified a specific (if not yet fully pinpointed)
real-JDK-class construction site as the actual hang.

## 2026-07-26 AOT follow-up 10 -- Blocker 2 FIXED, the CGLIB cross-test residual FIXED (40/40), a third blocker behind them FIXED; `endToEndTestsForBeanOverrides` runs all 175 tests again (13 failures, one family, fully characterised)

Worktree `/data/data/wt-testngresid-20260726` (branch `fix/aot-cluster-20260726`,
from `origin/dev` `95e4d9929`, with the concurrent session's Blocker-1 fix
`34201b21e` merged in). Azure host `20.83.144.174`, real JDK 25. No subagents,
per this task's standing instruction.

**Scoreboard**

| | before | after |
|---|---|---|
| `ApplicationContextAotGeneratorTests` | 38/40 | **40/40** |
| `AotIntegrationTests#endToEndTestsForBeanOverrides` | aborts on test class #1 (`failOnError=true`) | **runs all 175, 13 failures** |
| `test.context.testng.*` | 8x LOADERR | all discover and run (Blocker 1, concurrent session) |

Three genuine VM bugs fixed. Every one had been mis-hypothesised in this doc
before, and every one was found by reducing the failure to a standalone probe
rather than by reasoning about the Spring stack. The probes live in
`/data/data/aot20260726/src/` on the host.

### Fix 1: Blocker 2 -- `String.hashCode()` returns garbage from JIT-compiled code

**This doc's own hypothesis for Blocker 2 was wrong.** It read the
`NoSuchMethodError: java.lang.Class.getDeclaredMethods()` as "a method-RESOLUTION
failure specific to a dynamically-`defineClass`'d class's own constant pool". It
is nothing of the kind: the generated *class file itself* was corrupt, and the
corruption came from a JIT miscompilation of `String.hashCode()`.

Reduced to a 0.3s Spring-free probe (`CglibProbe.java`: one `Enhancer.create()`
on a two-method class). With JIT it died with `AbstractMethodError:
...$$FastClassByCGLIB$$...<init>(Ljava/lang/Class;)V has no Code attribute`;
with `--nojit` it passed. Dumping both runs' generated classes
(`-Dcglib.debugLocation=...`, which works on CratonVM) showed the JIT run's
class files were 60% LARGER than the interpreter's (12297/6124/4031 bytes vs
7321/5830/2585 -- the interpreter's sizes match HotSpot's exactly), with a
**duplicated constant pool** (three separate `Utf8 java/lang/Object` entries)
and, on one class, a `Code` attribute whose `attribute_name_index` was **0**
(`javap` renders it as `: length = 0x12 (unknown attribute)`).

`CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_SKIP` narrowed it, with no rebuild, to
exactly one method: `org.springframework.asm.SymbolTable.hash` -- six tiny
private static overloads that all reduce to `0x7FFFFFFF & (tag +
value.hashCode())`. A pure-Java probe of that shape then showed the defect with
no ASM involved at all: from ~1k iterations on (i.e. once the method tiers up),
`String.hashCode()` returned 0 or garbage, and `String.length()` intermittently
did too.

**Root cause** (`jit/src/lib.rs`, `StringFieldLayout::new`): the compact-layout
branch biased a *reference* field's offset by `-FIELD_CELL_PAYLOAD64_OFFSET` --
so the x64 call sites' own `+ FIELD_CELL_PAYLOAD64_OFFSET` lands on the bare
pointer -- but returned a *primitive* field's offset unbiased. Compact-layout
primitives are just as bare: natural width, naturally aligned, no tag word (see
`classloading/src/class.rs`'s `CompactLayout` builder and
`types/src/field_layout.rs::read_compact_field`). Every `x64.rs` consumer adds
its own `+ FIELD_CELL_PAYLOAD32_OFFSET`, so each read landed 4 bytes past the
field. For JDK 25's `java/lang/String` (`value:[B @0`, `coder:B @8`,
`hash:I @12`, `hashIsZero:Z @16`) that put the `coder` read on `hash` and the
`hash` read on `hashIsZero` + padding. `hashCode()` returned whatever garbage
sat there or, when that read zero, recomputed the hash with `hash` misread as
`coder` -- shifting the char count right by `hash & 31` and returning 0.

Why the blast radius stayed narrow enough to go unnoticed: the intrinsic only
fires on a **statically-String-typed** receiver. `HashMap`, `Objects.hash`,
`Arrays.hashCode` and friends all call `invokevirtual java/lang/Object.hashCode`
and were unaffected. `SymbolTable.hash` is not so lucky -- it folds
`String.hashCode()` straight into the constant-pool dedup key, so once it tiered
up, dedup stopped working and (via `MethodWriter.putMethodInfo`'s
`addConstantUtf8(Constants.CODE)`) the `Code` attribute name index came out 0.
Every ASM- or cglib-generated class in the process was born corrupt.

Fixed by biasing primitives by `-FIELD_CELL_PAYLOAD32_OFFSET` in the same
branch. `String.coder` additionally now loads at its declared **8-bit** width:
at natural width it is one byte followed by alignment padding that nothing
zeroes, so the old 32-bit load folded that padding into the value (the legacy
16-byte tagged cell stores the same value little-endian and `coder` is only ever
0 or 1, so the narrow load is correct for both representations).

The `layout_constant_inventory` tripwire in `jit/src/lib.rs` -- added by the
2026-07-26 header-shrink audit precisely to catch a layout constant appearing in
a file where it never appeared before -- fired on the new
`FIELD_CELL_PAYLOAD32_OFFSET` use and forced the inventory and both
`docs/internal/arch-2026-07-26` files to be updated in the same change. It
worked exactly as designed.

Verified: `CglibProbe` passes, and the classes it generates are now
**byte-identical between JIT and `--nojit`** (and the same sizes HotSpot emits).

### Fix 2: the CGLIB cross-test residual -- a generated-name collision, not a cache-key problem

Follow-ups 8 and 9 both chased `config_enhancer_class_cache`'s key shape and
both concluded, correctly, that the key was fine and the double-MISS was "the
anomaly". The double-MISS was never the bug -- it is the *correct* answer for
two genuinely different `CglibConfiguration` classes loaded by two different
`@CompileWithForkedClassLoader` loaders. The bug is what happens next, and it
was visible all along in the unconditional `[CCE]` stderr line nobody had
grepped:

```
[CCE] enhance: define_class_full failed for ...CglibConfiguration$$SpringCGLIB$$0:
      IncompatibleClassChangeError { message: "... already defined by application
      loader" } — fallback to identity
```

`cce_enhance` numbers the generated `$$SpringCGLIB$$<n>` suffix per
`(super_loader_id, super_internal_name)` -- so the second loader's copy
correctly drew `$$0` from its own fresh counter -- but then called
`define_class_full(..., loader_id = 0, ...)`, hardcoded to the application
loader. A per-loader counter is only collision-free inside a per-loader
namespace, so the second `$$0` collided with the first and the code answered the
collision by **returning the original, un-enhanced class**. That is exactly the
two documented symptoms: `processAheadOfTimeUsesCglibClassForFactoryMethod`'s
`IllegalArgumentException: class ...CglibConfiguration is not an enhanced class`
(thrown by `Enhancer.registerStaticCallbacks` at AOT replay, on a class that
never got a `CGLIB$SET_STATIC_CALLBACKS`) and
`processAheadOfTimeWhenHasCglibProxyUseProxy`'s "Hello1" (the `@Bean` body
running twice, because the un-enhanced config class was instantiated directly).
It also explains the isolation asymmetry perfectly: one loader, no collision,
both pass.

Fixed by defining the enhancer subclass into `super_loader_id` -- the
superclass's own loader, which is where real cglib puts it -- so each loader's
first enhancement keeps the `$$0` name the tests hardcode in generated *source
text*. A bounded retry (burn the counter, regenerate) covers any remaining way
the name could still be taken, since handing back an un-enhanced class is never
the right answer.

**`ApplicationContextAotGeneratorTests` 38/40 -> 40/40**, with every
`[CCE] enhance: defined` line in the run landing on `$$SpringCGLIB$$0` and no
`already defined`/identity fallback anywhere.

### Fix 3: a real `StringBuilder.length()` returns 0 after any Mockito mock

With Blockers 1 and 2 gone, `endToEndTestsForBeanOverrides` reached real AOT
processing and aborted (`failOnError=true`) on the first `@MockitoSpyBean` class
with the *same* `NoSuchMethodError` symptom as Blocker 2 -- but for a different
reason, visible in the descriptor:

```
NoSuchMethodError method="java/lang/Class.getDeclaredMethods()[Ljava/lang/reflect/Method[];"
```

Note the malformed `[Ljava/lang/reflect/Method[];`. That is precisely what
`org.springframework.cglib.core.TypeUtils.map` emits when
`type.substring(0, type.length() - sb.length() * 2)` fails to strip the trailing
`[]` -- i.e. when `sb.length()` returns 0 for a `StringBuilder` that just had
one `[` appended. Because `MethodInterceptorGenerator`'s `GET_DECLARED_METHODS`
signature is a `static final` computed once, a single Mockito mock anywhere in
the process poisons **every** cglib proxy generated afterwards in that loader.

This is the **KNOWN REMAINING GAP** the 2026-07-23 MockitoBean session
documented and left open, reproduced here in three seconds
(`RealAfterMockLengthProbe.java`: mock a `StringBuilder`, then use a real one).
Root cause, though, is not what that session assumed. `sb_set_count`'s
3-or-more-slot branch writes the JDK 9 layout `value/coder/count` with `count`
at slot 2. **JDK 25's `AbstractStringBuilder` declares FOUR instance fields --
`value @0, coder @1, maybeLatin1 @2, count @3`** -- so slot 2 is `maybeLatin1`
and the field genuinely named `count` was never written at all. That is
invisible while CratonVM's natives are the only readers (they agree with each
other on the wrong slot), and fatal the moment real `AbstractStringBuilder`
bytecode runs against one of these objects -- which is what Mockito's in-place
redefinition of `StringBuilder`/`AbstractStringBuilder` causes, since `length()`
then cedes to the woven advice whose "not mocked" fallthrough is the original
`getfield count:I`.

Fixed additively: keep the index-based writes (every native in `lang_string.rs`
reads them back, and the unit-test `NativeContext` mock has no class model to
resolve names against -- it answers `Int(0)` for any name it does not know,
which is indistinguishable from a real zero count) and mirror the value into
`count` by name as well.

Two approaches were tried and rejected first, both worth recording:
- Resolving `count` by name on the READ side too -- breaks 12 `lang_string`
  unit tests for the mock-context reason above.
- Fixing it at the dispatch layer: force the registered native for a "real
  carrier" the way the `java/net/HttpURLConnection` exemption does, keyed on a
  null field 0. **Does not work here** -- CratonVM allocates a Mockito-mocked
  `StringBuilder` through the same synthetic path as a real one, so a mock's
  backing buffer is non-null too; the exemption fired for mocks as well and
  broke `verify(mock).length()` and `when(mock.length())`. Anyone reaching for
  the HttpURLConnection trick on another class should check this first.

### `endToEndTestsForBeanOverrides`: 175 tests, 13 failures, all one family

The run now completes (`ms=910642` under a load average of ~28; budget
generously). All 13 failures are Family A -- `@MockitoBean`/`@MockitoSpyBean`
**by-name** lookup for **constructor parameters** -- in exactly three classes:

| class | failures |
|---|---|
| `...mockito.constructor.MockitoBeanByNameLookupForConstructorParametersIntegrationTests` | 7 |
| `...mockito.constructor.MockitoSpyBeanByNameLookupForConstructorParametersIntegrationTests` | 5 |
| `...mockito.typelevel.MockitoBeansByNameIntegrationTests` | 1 |

All shaped like:

```
ParameterResolutionException: Failed to resolve parameter [... ExampleService service2]
in constructor [...]: No qualifying bean of type '...ExampleService' available:
expected single matching bean but found 4: s1,s2,s3,s4
```

**New and important**: all three classes pass **100% in normal (non-AOT) mode**
on this same binary (7/7, 5/5, 1/1). So this is not the field-vs-constructor
override bug follow-up 7 fixed -- it is specific to what AOT *replay* does with
a by-name override, i.e. the AOT-generated bean definitions do not carry the
name-based replacement, leaving all the original candidates in play for
by-type constructor autowiring. That is a far tighter starting point than the
"17 failures, two families, unisolated" this doc has carried since follow-up 7.

**Family B appears to be gone.** The `AssertionFailedError: expected: null but
was: ""` shape that motivated follow-ups 9b/9c does not occur anywhere in this
run (zero `AssertionFailedError`s of any kind). The most likely explanation is
Fix 1: a `String.hashCode()` that returns 0 for arbitrary strings will corrupt
any map keyed by String, and the AOT generator is full of them. Not proven --
recorded as an observation, to be reconfirmed on the next run.

### Still open in the AOT cluster

1. The 13 Family-A failures above (AOT-replay-only, three named classes).
2. `TestContextAotGeneratorIntegrationTests`' `Arrays.stream`/
   `Spliterators.spliterator` hang (follow-up 9c) -- not revisited this session;
   note that follow-up 9c's own runs predate Fix 1, and a corrupted
   `String.hashCode()` is a plausible contributor to a hang in
   `AccessControl.lowest`'s map-heavy call chain, so **re-measure before
   re-investigating**.
3. `cglib_enhancer::fb_ref_bytecode_tests::fb_ref_splice_shifts_exception_table_by_exactly_8_bytes`
   -- still failing, still pre-existing on `origin/dev` (confirmed again this
   session by running the same test against this file's pre-change contents),
   still just stale hardcoded byte-length constants.
