# CratonVM Spring Suite — Consolidated Open Bugs
**Latest Update: July 15, 2026**

## Executive Summary
Multiple major bug clusters have been successfully resolved (including the JIT SIGSEGV in Groovy, the `java.home` Locale regression, the `Semaphore` deadlock, and dozens of classloader visibility/AOT fixes).

This document tracks the **genuine remaining failures**.

---

## 1. Deep-Dive Investigations (Root-Caused, Pending Fix)

*   **Mockito `spy()` StackOverflowError** (`context.annotation.ImportSelectorTests`)
  *   **Status**: **OPEN** (5/9 methods SOE, reconfirmed 2026-07-15 on a binary containing the mockk
    fix below — this is a **different root cause** than the mockk sibling, which is now FIXED).
  *   **2026-07-15 root-cause sharpening (supersedes the older hypotheses below)**: full recursion
    cycle captured (`KRUN_STACK=1`, log at `/data/tmp/mockk-tmp/importsel2.log` on the Azure host):
    `MockMethodAdvice.handle` (intercepting `DefaultSingletonBeanRegistry.registerSingleton` on the
    spy) -> `CallsRealMethods.answer` -> `InterceptedInvocation.callRealMethod` ->
    `SerializableRealMethodCall.invoke` -> `MockMethodAdvice.tryInvoke` ->
    `ModuleMemberAccessor`/`InstrumentationMemberAccessor.invoke` (MethodHandle-based) which
    dispatches VIRTUALLY and lands in the subclass override
    `DefaultListableBeanFactory.registerSingleton:1491` -> its `super.registerSingleton` ->
    `DefaultSingletonBeanRegistry.registerSingleton:142` -> intercepted AGAIN -> same cycle forever.
    On HotSpot the super-method frame is never intercepted: `MockMethodAdvice.enter` first checks
    `dispatcher.isOverridden(mock, origin)` (a ByteBuddy `MethodGraph` compiled over
    `instance.getClass()`) and bails to real code when the runtime class overrides `origin`. Prime
    suspect: `isOverridden` computing `false` under CratonVM (reflection/MethodGraph disagreement
    about the retransformed hierarchy) — NOT the ThreadLocal guard (CratonVM ThreadLocal semantics
    probe-verified correct, TLProbe 2026-07-15).
  *   ~~**Old hypothesis**: the `ThreadLocal`-based `MockMethodAdvice$SelfCallInfo.checkSelfCall`
    guard fails to match on CratonVM~~ — never verified; ThreadLocal itself ruled out.
  *   **Next step**: standalone probe that spies a 2-level hierarchy (subclass override calling
    `super.<same method>`), invokes the parent method through the spy, and instruments what
    `MockMethodAdvice.isOverridden` computes on CratonVM vs HotSpot.

*   **mockk `hashCode()` StackOverflowError — ~10 Kotlin reactive classes** — **FIXED (2026-07-15,
    dev `9dce07a5`)**, plus a host-environment trap that will bite again if undocumented:
  *   Affected classes (all previously 100% SOE): `WebTestClientExtensionsTests`,
    `WebClientExtensionsTests`, `ServerResponseExtensionsTests`, `ServerRequestExtensionsTests`,
    `RenderingResponseExtensionsTests`, `ClientResponseExtensionsTests`,
    `RSocketRequesterExtensionsTests`, `WebClientObservationTests`, `CoExchangeFilterFunctionTests`,
    `InvocableHandlerMethodKotlinTests`.
  *   **True root cause (NOT method dispatch)**: mockk only recurses in its **agent-less** mode.
    `JvmMockKAgentFactory$init$Initializer` picks the handler map via
    `MockHandlerMap.create(instrumentation != null)`: with instrumentation ->
    `WeakMockHandlersMap` (identity-keyed `JvmMockKWeakMap`, never calls `hashCode()`); without ->
    `SynchronizedMockHandlersMap` (plain `Collections.synchronizedMap(LinkedHashMap)`). The
    generated mock subclass intercepts `hashCode()` on BOTH VMs by design (`SubclassInstrumentation`
    uses `ElementMatchers.any()`), so in agent-less mode `handlers.get(mock)` -> `mock.hashCode()`
    -> interceptor -> `handlers.get(mock)` -> SOE. The earlier claim "CratonVM routes the mock's
    `hashCode()` through the interceptor where HotSpot dispatches to the real `Object.hashCode`"
    was WRONG — both VMs intercept it; HotSpot survives only because it normally has instrumentation
    and the identity-keyed map. (Reproduced the identical SOE **on HotSpot jdk25** by letting the
    boot jar fail.)
  *   **Why CratonVM ended up agent-less**: the Azure host's root fs (`/tmp`) flaps at ~100% full;
    mockk's `BootJarLoader` writes its dispatcher boot jar via `File.createTempFile` +
    `JarOutputStream`, the jar write hits ENOSPC, mockk logs "Can't inject boot jar." at TRACE only
    and silently downgrades. Two genuine CratonVM parity bugs then sealed the failure:
    **(1)** the `File.createTempFile` (both overloads), `Files.createTempFile` and
    `Files.createTempDirectory` natives read `std::env::temp_dir()` directly and **ignored
    `-Djava.io.tmpdir`**, so the standard redirect escape hatch was a no-op; **(2)** they swallowed
    creation errors (`let _ = std::fs::File::create(..)`) instead of throwing `IOException` (real-JDK
    contract; mockk falls back to a CWD boot jar when `createTempFile` throws). Both fixed in
    `9dce07a5` (`native-builtins/src/phases_late.rs`, helpers `jdk_temp_dir` /
    `jdk_create_temp_file`).
  *   **Host-env trap (still live)**: with `/tmp` full, HotSpot fails these classes too, just
    differently — the attach socket `/tmp/.java_pid<pid>` cannot be created, so
    `ByteBuddyAgent.install()` throws (mockk does NOT catch it) ->
    `ExceptionInInitializerError: io.mockk.impl.JvmMockKGateway`. Any suite run comparing the two
    VMs during a full-`/tmp` window is comparing two env failures. CratonVM implements
    `Instrumentation` internally (no attach socket), so post-fix CratonVM passes where host HotSpot
    currently cannot. **Harness requirement on this host: run mockk/Mockito-heavy classes with
    `-Djava.io.tmpdir=/data/tmp/<writable>`** (honored by CratonVM as of this fix).
  *   Verification (fixed binary + `-Djava.io.tmpdir=/data/tmp/mockk-tmp`, KRun):
    WebTestClient 10/10, ServerRequest 29/29, ServerResponse 13/13, ClientResponse 16/16,
    RenderingResponse 1/1, RSocketRequester 13/13, WebClientObservation 9/9,
    CoExchangeFilterFunction 1/1, InvocableHandlerMethodKotlin 40/40 — all OK. A/B control: unfixed
    binary, identical flags -> 10/10 SOE.
  *   Residual: `WebClientExtensionsTests` 20/32 (SOE gone). The 12 failures are two separate,
    pre-existing families: (a) `IllegalStateException: Unable to create proxy for sealed class
    interface org.springframework.http.HttpStatusCode, no subclasses available` — mockk resolves
    `KClass.sealedSubclasses` (kotlin-reflect metadata family); (b) mockk `verify` matcher failures
    (`... was not called`), same family as the tracked `RestClientExtensionsTests` residual.
  *   Probe kit: `/data/data/wt-mockk-dispatch-20260715/probes/` — `MkProbe.java` (agent-init +
    hashCode chain with a printing `MockKAgentLogFactory`; the init TRACE lines name the exact
    failing step), `BootProbe.java` (boot-jar append + null-loader `forName`), `TmpProbe.java`
    (`java.io.tmpdir` honoring).
*   **`@Import` attribute CCE across `@CompileWithForkedClassLoader`** (`web.service.registry.ImportHttpServiceRegistrarTests`)
  *   **Status**: **OPEN** (2/5 methods: `basicListingWithAot`, `basicScanWithAot` — `ClassCastException: java.lang.Class cannot be cast to [Ljava.lang.String;` at `ConfigurationClassParser$SourceClass.getAnnotationAttributes`)
  *   **2026-07-15 update**: reproduces SOLO in ~1s (`/data/tmp/aotfix-runs/MethodRun.java` single-method launcher on the Azure host). The 2026-07-14 SoftReference/GC-relocation hypothesis is now DOUBTED: this failure survived eight classloader-identity fixes, and three focused probes (plain `@Import` reflection, forked-loader variant, `@Import` as meta-annotation on a repeatable annotation type — `ImportProbe2.java`) all PASS. The divergence is somewhere in the full `ConfigurationClassParser`/`MergedAnnotations` path for the repeatable `@ImportHttpServices` container under a forked loader.
*   **STOMP Message Hang** (`web.socket.messaging.StompWebSocketIntegrationTests`)
  *   **Status**: **OPEN** (TIMEOUT) — functional gap in STOMP routing/delivery, not a VM deadlock.

---

## 2. AOT / In-Memory Javac Cluster — LARGELY RESOLVED (2026-07-15)

The 11-class AOT cluster turned out to be a FAMILY of **classloader-identity
bugs**: `@CompileWithForkedClassLoader` re-defines the whole framework in a
fork loader (and every `TestCompiler` compile uses a fresh
`DynamicClassLoader`), so any name-global lookup inside the VM could resolve
the WRONG same-named copy. Eight fixes landed on
`fix/spring-aot-cluster-20260715` (commits `7c5aa7ce`, `5833302e`):

1. Link-time Pass-3 re-verification removed (name-indexed adapter threw
   spurious `VerifyError`s; define-time Pass 3 is loader-aware and
   authoritative). Also ~2x faster on javac-heavy tests.
2. `LambdaCallSite` records the loader-resolved functional-interface
   `ClassId` at indy bootstrap; non-SAM (default) interface methods on lambda
   proxies dispatch through it (fork-side `ArgumentCodeGenerator.and()`
   chains no longer produce app-side javapoet `TypeName`s).
3. `loader_namespace_id` keyed by loader OBJECT with GC reconcile (identity
   hashes recur; fresh per-compile loaders inherited dead siblings'
   namespaces -> stale `Test__Injector` CCEs).
4. Field/method/parameter annotation TYPES resolve through the declaring
   class's loader (fork-side `@Autowired` members no longer materialize
   app-loader annotation types that fail identity comparisons).
5. Jars appended via `Instrumentation.appendToBootstrapClassLoaderSearch`
   are recorded; the synthetic-mode loadClass defer-gate serves them via
   parent delegation.
6. `defineClass1` returns the already-defined BOOTSTRAP copy for
   appended-jar classes (Mockito `MockMethodDispatcher` null-loader assert).
7. `Class.forName` on Spring's `DynamicClassLoader` reroutes to the parent
   ONLY when the parent is the forked test loader (CGLIB `$$SpringCGLIB$$`
   classes defined into a plain DCL were CNFE-invisible).
8. JVMS 5.3 chain-scoped flat-store fallback in the synthetic-mode base
   delegation.

**Full-cluster validation (2026-07-15, one VM per class, real JDK 25, JIT on,
900 s watchdogs, results `/data/tmp/aotfix-runs/V7_shard*/results.tsv`):**

| class | doc baseline (2026-07-14) | now |
|---|---|---|
| `AutowiredAnnotationBeanRegistrationAotContributionTests` | FAIL 14/1 | **OK 14/14** |
| `CommonAnnotationBeanRegistrationAotContributionTests` | FAIL 8/1 | **OK 8/8** |
| `BeanDefinitionPropertiesCodeGeneratorTests` | FAIL 47/0 | **OK 47/47** |
| `InstanceSupplierCodeGeneratorTests` | FAIL 26/0 | **OK 26/24 (2 skip)** |
| `BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | LOADERR/ABEND | **OK 44/44** |
| `DefaultBeanRegistrationCodeFragmentsTests` | (was fixed) | **OK 19/19** |
| `GroupsMetadataValueDelegateTests` (WritableContent residual) | FAIL | **OK 8/8** |
| `ScopedProxyBeanRegistrationAotProcessorTests` | FAIL (3 methods) | **OK 5/5** |
| `PersistenceManagedTypesBeanRegistrationAotProcessorTests` | FAIL | FAIL 2/0 (host lacks JDK 24+, see below — not a VM bug) |
| `TestClassScannerTests` | TIMEOUT 600 s | **completes 177 s** (7/7 or flaky 7/6) |
| `TestCompilerTests` | TIMEOUT 600 s+ | **completes 40 s**, FAIL 22/18/4 |
| `ApplicationContextAotGeneratorTests` | ABEND (CGLIB load) | discovers+runs 40 methods (see residuals) |
| `BeanDefinitionMethodGeneratorTests` | FAIL 34/3 | **OK 34/34** |
| `ConfigurationClassPostProcessorAotContributionTests` | FAIL 20/8 | FAIL 20/18/2 |
| `PersistenceAnnotationBeanPostProcessorAotContributionTests` | FAIL 8/0 (NCDFE) | FAIL 8/2/6 (Mockito attach residuals) |
| `TestContextAotGeneratorIntegrationTests` | FAIL 4/0 @393 s | (see residuals) |
| `BeanRegistrationsAotContributionTests` | TIMEOUT | TIMEOUT (throughput, see below) |

### Remaining OPEN residuals in the AOT cluster

*   `BeanRegistrationsAotContributionTests` — **TIMEOUT even at 1500 s**, 100%
    CPU, steadily progressing (NOT a deadlock). Stack samples put the main
    thread repeatedly in Mockito's inline-mock-maker constructor interception
    (`InlineDelegateByteBuddyMockMaker.lambda$new$2/3`) plus GC frame-root
    scanning — an interpreter-throughput problem under constructor
    instrumentation, needing perf work rather than a correctness fix.
*   **`ConfigurationClassPostProcessorAotContributionTests` — 2 residuals in
    `BeanRegistrarTests` under fork, ROOT-CAUSED 2026-07-15 (not yet fixed —
    the fix is architecturally bigger than this cluster's other loader-identity
    bugs, see below).** `applyToWhenIsPackagePrivate`/
    `applyToWhenIsPackagePrivateAndImportAware` throw
    `IllegalArgumentException: Could not generate code for
    <com.example.TestTarget__TestCode>::applyBeanRegistrars: parameter 0 of
    type org.springframework.beans.factory.ListableBeanFactory is not
    supported` — Spring's `DefaultMethodReference.addArguments` (javapoet
    `TypeName`-based, NOT a `Class` object) fails to match the captured
    `ListableBeanFactory` parameter type against the test's own
    `ArgumentCodeGenerator.of(ListableBeanFactory.class, ...)`.

    Traced (env-gated `eprintln!` in `resolve_class_loader_aware`,
    `vm/src/runtime/interpreter.rs`) down to a DIFFERENT class-identity gap
    than the rest of this doc's fixes: `@CompileWithForkedClassLoader`'s
    `CompileWithForkedClassLoaderClassLoader` correctly gets its OWN
    `ClassId` for the directly-`Class.forName`'d nested test class
    (`BeanRegistrarTests`, confirmed via trace: `get_loader_id=UserDefined(N)`,
    defining-loader registry entry present) — but `BeanRegistrarTests`'s
    ENCLOSING class (`ConfigurationClassPostProcessorAotContributionTests`)
    resolves to the STALE, original app-loader `ClassId`
    (`get_loader_id=Application`, no defining-loader entry at all), and
    EVERYTHING transitively touched through it (`ConfigurationClassPostProcessor`,
    `ConfigurationClassBeanDefinitionReader`, `ListableBeanFactory` itself,
    …) inherits that same stale, app-loader identity. The nested test
    method's OWN `ListableBeanFactory.class` literal, by contrast, resolves
    correctly through the fork loader (since `BeanRegistrarTests` itself IS
    loader-correct) — giving two DIFFERENT `ListableBeanFactory` copies to
    compare, hence the "not supported" mismatch.

    Root cause of the ENCLOSING-CLASS gap: `Vm::declaring_class`
    (`vm/src/vm/vm_exec.rs`, backs `Class.getEnclosingClass()`/
    `getDeclaringClass()`) resolves the outer-class name via
    `cm.find_class_by_name(&ic.outer_class)` — a FLAT, GLOBAL, name-only
    lookup that never considers the INNER class's own defining loader. Any
    nested class redefined under an isolating loader (this cluster's
    `@CompileWithForkedClassLoader`, `DynamicClassLoader`, or a future
    Tomcat/Hibernate/WildFly custom loader) will have `getEnclosingClass()`
    silently hand back whichever loader's copy of the same-named outer class
    was registered FIRST — completely bypassing the `resolve_class_loader_aware`
    / `CRATONVM_LOADER_AWARE_RESOLUTION` machinery that already correctly
    handles ORDINARY bytecode-level `ldc`/`checkcast`/`new` class references
    (confirmed: adding an analogous narrow carve-out to
    `should_use_loader_initiated_resolution` for these two Spring test-tools
    loader classes, mirroring the existing `is_groovy_class_loader` pattern,
    correctly activates for `BeanRegistrarTests` itself — but does NOT fix
    this test, because the enclosing-class lookup never goes through that
    gate at all).

    NEXT STEP (not attempted this round — genuinely bigger scope than this
    doc's other fixes): make `Vm::declaring_class` loader-aware. `declaring_class`
    currently takes only `&self` (a `class_manager` read lock, no thread
    context), so it can only PREFER an already-loaded same-loader match (a
    `class_defined_by_loader_exact`-style lookup keyed by the inner class's
    OWN loader) before falling back to the global one — it CANNOT actively
    drive that loader's `loadClass()` for an outer class it hasn't loaded yet
    (that needs `&mut thread`/`NativeContext`, i.e. the same re-entrant-call
    machinery `drive_defining_loader_load` already has, threaded through a
    different call path). Given `getEnclosingClass()`/`getDeclaringClass()`
    is used pervasively by reflection-heavy frameworks well beyond this AOT
    cluster, this needs its own careful, isolated soak — do not bundle it
    with an unrelated fix.
*   `PersistenceAnnotationBeanPostProcessorAotContributionTests` — 8/2/6.
    Post-fix the forked Mockito path advanced: now (a) fork attach via
    `PremainAttachAccess` -> "Byte Buddy agent is not initialized", and (b) a
    NEW ByteBuddy generics failure past the dispatcher: `IllegalArgumentException:
    Cannot resolve T from class ...EntityManagerFactory$MockitoMock$...`.
*   `PersistenceManagedTypesBeanRegistrationAotProcessorTests` — **NOT a
    CratonVM bug: host environment gap.** Both `processEntityManagerWithPackagesToScan`
    and `contributeJpaHints` hit `NoClassDefFoundError: java/lang/classfile/ClassFile`
    inside `ClassFileMetadataReader.parseClassModel` when run in REAL-JDK
    mode against whatever `java` is on `PATH`. `java.lang.classfile.ClassFile`
    is a JDK 24+ finalized API (JEP 484, preview in 22/23, absent before
    that) — the Azure worktree host's only installed JDKs are 17 and 21
    (`/usr/lib/jvm/java-{17,21}-openjdk-amd64`), so this class genuinely does
    not exist there; CratonVM real-JDK mode is behaving exactly like real
    JDK 21 would. The 2026-07-14 baseline's "OK 2/2" almost certainly ran
    with a newer real JDK (or synthetic-JDK mode) available in whatever
    environment produced it. Fix is environmental (point `--java-home` at a
    JDK 24+ install, or install one) — not a code change, and out of scope
    for this document until a suitable JDK is available on the run host.
*   `InstanceSupplierCodeGeneratorKotlinTests` — 4/0/5, all
    `ClassCastException: kotlin.reflect...protobuf.SmallSortedMap$Entry cannot
    be cast to java.lang.reflect.Field / AnnotationSpec` (separate
    heap/collection-identity family, kotlin-reflect metadata parsing).
*   `TestCompilerTests` — 4 residuals: package-private access via
    `@CompileWithTargetClassAccess`-style flows + additional-class references
    (`CompilationException: Unable to compile source`).
*   ~~`aot.nativex.feature.ThrowawayClassLoaderTests`~~ **FIXED (2026-07-15,
    commit `56a98cc4`)**. `native-builtins/src/classloader_real.rs`'s
    `cl_real_load_class_base` — the REAL-JDK-mode counterpart of the
    synthetic-mode function fixed earlier in this doc's round 2 — had the
    same missing JVMS 5.3 chain-scoping: `new ClassLoader(null){}.loadClass(x)`
    resolved app classes directly from the flat store even though the
    loader's real parent chain never reaches a built-in loader. Ported the
    same `scoped_user_chain` gate. Full class now 2/2 OK.
*   ~~`BeanDefinitionMethodGeneratorTests`~~ **FIXED (2026-07-15, commit
    `d017aa36`).** Bisection of the
    `generateBeanDefinitionMethodWhenHasExplicitResolvableType` residual
    (`MethodRun.java` accepts N method names to run together in one process)
    showed the failure was **COUNT-dependent, not content-dependent**: 9
    preceding `TestCompiler` compile cycles before the target passed; 10
    failed, regardless of which methods supplied the 9th/10th cycle. Root
    cause: `gc_reconcile_defining_loaders`
    (`native-builtins/src/classloader.rs`) DROPS a class's defining-loader
    registry entry once that loader is collected, and `cid_visible_mirror`
    reads a missing entry as "never restricted, visible to everyone" — the
    same answer it gives a class that was never loader-scoped. Once the Nth
    cycle's `DynamicClassLoader` was collected, its generated companion class
    silently became visible to every OTHER loader, so the next cycle's
    `DynamicClassLoader` reused the stale, wrong-scenario copy instead of
    generating its own. Fixed by tombstoning pruned class-ids in a permanent
    orphaned set, checked before the live-registry lookup, so a class whose
    defining loader died stays invisible to everyone forever (matches real
    unloading semantics). Full class now 34/34 OK. This is a DIFFERENT root
    cause than originally hypothesized here — it does NOT explain the
    `PersistenceAnnotationBeanPostProcessorAotContributionTests` ByteBuddy
    `NoSuchMethodError` family (confirmed unaffected, still 8/2/6 after this
    fix); that remains open and unrelated.

---

## 3. Untriaged Clusters & Per-Class Details

*Note: this section was accidentally truncated in the 2026-07-14 rewrite; the
full 1013-line per-class detail lives in git history as
`CRATONVM-SPRING-GENUINE-BUGLIST-125.md` (deleted in `ccab25c6`). 21 classes
related to `HIB-CV-32` heap corruption remain filtered out as load-dependent
side-effects tracked separately. The still-open non-AOT clusters from that
list (WebFlux backend failures, Groovy scripting cluster, WebFlux
EMPTY-discovery family, 6 found=0 ABENDs, and the per-class FAIL details)
are unchanged by the 2026-07-15 AOT work — consult the historical doc.*

### WebFlux Backend-Specific Failures
(`web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests`, `RequestMappingMessageConversionIntegrationTests`)
*   **Update 2026-07-15 (reactive-cluster session, branch `fix/reactive-cluster-20260715`)**: the whole
    per-backend residual cluster (Jetty `MimeTypes$Mutable` NCDFE / Tomcat `StandardServer`
    LifecycleException / Reactor Netty "failed to create a child event loop") was root-caused to
    process-global poisoning chains and fixed: `CharBuffer.getArray` AIOOBE (missing `Buffer.address`
    seed on `asCharBuffer()` views) poisoned `jdk.internal.icu` → `java.net.IDN` → Netty's buffer
    stack; a JIT NPE in `sun.misc.Unsafe.putOrderedLong` (the native `<clinit>` shadow never populated
    `theInternalUnsafe`; the interpreter's C11/C16 null-receiver rescue papered over it but compiled
    code has no such rescue) killed `ByteBufUtil.<clinit>` at Netty's `MpmcArrayQueue(4096)` init loop.
    **`CrossOriginAnnotationIntegrationTests` is now fully OK (68/68)** on all backends.
*   ~~**Current Residual (OPEN)**: `RequestMappingMessageConversionIntegrationTests` (160 tests) runs but
    is pathologically slow — steady progress (fresh `AnnotationConfigApplicationContext` + HTTP server
    per test; watchdog stack dumps show active bean creation, no deadlock) yet does not finish within
    1800s (HotSpot: 13s). Needs a dedicated perf investigation.~~
    **PARTIALLY FIXED (2026-07-15, commit `96c8a57f`, branch `fix/xerces-perf-20260715`)**: root-caused
    and fixed a genuine, general CPU-dispatch bug (JDK-internal bytecode — including Xerces — never
    consulted the interpreter's monomorphic invoke cache; see the detailed writeup further down in
    this document, section 2, same bullet). Verified via an isolated repro: 2.7x-3.4x faster
    per-parse. **Full-class wall-clock improvement on this specific test NOT confirmed** — see the
    detailed entry below for the honest caveat (host-contention-confounded A/B comparison, and
    evidence the per-test cost here is dominated by something other than the bug fixed).

## 4. Reactive cluster session 2026-07-15 (branch `fix/reactive-cluster-20260715`)

Full 295-class reactive sweep (`web.reactive.*`, `messaging.rsocket.*`, `test.web.reactive.*`,
`http.server.reactive.*`, `http.client.reactive.*`, reactive tx/core classes) vs a same-day HotSpot
baseline. Baseline on the 2026-07-15 dev tip: 178 OK / 50 FAIL / 12 LOADERR / 5 ABEND / 4 TIMEOUT /
46 EMPTY. After the session's 7 VM fixes (missing `Buffer.address` on `asCharBuffer()` views;
`theInternalUnsafe` never populated by the `sun/misc/Unsafe` clinit shadow — JIT-compiled
`putOrderedLong` NPE; mutable-`ArrayList`-typed `Collections.EMPTY_LIST/MAP/SET` singletons that
kotlin-reflect's shaded protobuf mutated in place, corrupting `emptyList()` process-wide; speculative-BCE
loop-header guard missing the null-array check — freemarker `TemplateElement.setChildren` SIGSEGV; raw
pointer dereference of tagged Unsafe-arena handles in the TLS engine's direct-buffer accessors —
`SSLEngine.unwrap` SIGSEGV; `cratonvm/net/HttpBodyReplaySubscription` not declaring
`Flow$Subscription` — 40 sub-test failures on the `[2] JDK` WebClient connector; identity `finisher()`
on JOINING/COUNTING collectors when Reactor drives the raw Collector protocol) the sweep reaches
**HotSpot parity minus the residuals below** (the only EMPTY classes are the same 4 abstract classes
HotSpot reports EMPTY, and `ResourceWebHandlerTests` fails the same single
`servesResourcesFromFileSystem` test on both VMs).

An 8th fix landed during final validation: `java.net.URI`'s construction-time field writes
(`uri_store_named`) and the `getScheme`/`getRawSchemeSpecificPart` raw-string fallbacks treated ANY
first colon as a scheme delimiter, so a relative reference with a colon in its first path segment
(`/redirect:account`) parsed as `scheme="/redirect", path="account"`. Spring's view-resolution tests
derive the default view name from the request path, so `ViewResolutionResultHandlerTests.
defaultViewNameWithRedirectPrefixFails` (the FAIL this doc has tracked since the 516-class runs)
resolved the wrong view and completed instead of erroring. Scheme detection now mirrors the real JDK
parser (first stop char among `:/?#` must be `:`, ALPHA-start + alphanum/`+`/`-`/`.` name — the same
rule `uri_scheme_name_fail_index` already enforced for exceptions). The class is now 11/11 OK.

**RETRACTED 2026-07-15**: the "batch-context `<clinit>` contamination" theorized below (classes failing only in batched suite runs, passing solo) was investigated further and is **NOT a CratonVM bug**. Root cause was Azure-host environment corruption, confirmed live: (1) `/home/victor/jdk25` (the symlink itself) had vanished mid-session — every `--java-home ~/jdk25` invocation failed with `path does not exist`, producing zero test output, easily misread as a VM hang; (2) separately, `/tmp` (the directory itself, not just stale contents) had vanished — `Tomcat.initBaseDir()` threw `IllegalStateException: Unable to create the directory [/tmp]`, which looks exactly like a filesystem native bug but is the OS directory missing. After `ln -sf /data/data/jdk25-real ~/jdk25` and `mkdir -p /tmp && chmod 1777 /tmp`, the exact same 8-class batch that previously showed `SseIntegrationTests` failing 9/48 (deterministically, 3/3 reruns) now passes 42/48 with 6 aborted, byte-for-byte matching the HotSpot baseline shape, and `DefaultRenderingBuilderTests` never reproduced its `ExceptionInInitializerError` again across 3 clean reruns of the identical batch order on the identical binary. See `azure-host-disk-full-flapping-20260715.md` memory (Claude's memory system) for the full host instability catalog.

**Remaining OPEN reactive residuals:**
*   `http.client.reactive.ClientHttpConnectorTests` — **PARTIALLY FIXED 2026-07-15**
    (`fix/jdkclient-patch-hang-20260715`, commit `8e47b8a9`). Root-caused ONE genuine mechanism: the
    `java.net.http.HttpClient` native implementation (RE.5, `net_phase_e.rs`) performs its
    "async" `sendAsync()` synchronously on the calling thread via raw `TcpStream`
    connect/write/read (30s socket timeout) and a request-body `Publisher`-draining condvar wait
    (10s), neither of which was bracketed with `begin_blocking_region()`/`end_blocking_region()` —
    unlike every other blocking native I/O call in the same file. A concurrent Stop-The-World pause
    (GC/JIT takeover) then waits indefinitely on this uncooperative thread, while the SAME pause is
    what freezes the real Java thread on the other end of the socket (e.g. MockWebServer's response
    dispatcher) that this thread is blocked waiting to hear from — a genuine deadlock. Live capture
    (`CRATONVM_DBG_RE5=1`, new diagnostic) caught it in the act: a `DELETE` request's write
    succeeded, then the response read blocked the full 30s and failed with `WouldBlock`, with a
    `"STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1
    taken=0"` warning firing mid-block. Confirmed CratonVM-specific and connector-specific:
    HotSpot ran the identical 32-request sequential-`MockWebServer` probe (`ReactorNetty`/`Jetty`/
    `HttpComponents`/`Jdk` × 8 HTTP methods each, reusing one connector instance per type) 8/8 clean;
    CratonVM hit the hang on ~2/13 attempts, always on the `Jdk` connector, never the other 3 (which
    don't route through this raw-socket path). Fixed both blocking sites; verified 15/15 clean on the
    same probe post-fix, `cargo test -p cratonvm-native-builtins --lib` 2996/0.
    ~~**Still OPEN residual investigated 2026-07-15**~~ (`fix/httpconn-residual-20260715`, commit
    `91cb806c`). Bisected the "which of the other 3 connectors" question with a
    `FullMatrixProbe`-style stress harness plus a from-scratch JUnit launcher driving the real
    `ClientHttpConnectorTests` class (49 sub-tests) directly under CratonVM, both stress-run
    dozens of times with `sudo gdb -p <pid> --batch -ex 'thread apply all bt'` snapshots captured
    live on reproduced hangs (Jetty `TRACE`, Jdk `OPTIONS`/`DELETE` all reproduced the shape).
    **Found and fixed two genuine instances of the same missing-`begin_blocking_region` bug
    pattern**, but in the *shared* `java.net.Socket`/`java.net.ServerSocket` implementation
    (`native-builtins/src/plain_socket.rs`, JDK13+'s `NioSocketImpl` backing both classes) rather
    than in any one connector's own code — `java.net.ServerSocket.accept()` is what MockWebServer
    itself uses to accept every connection for all 4 connector cases:
      - `socket_accept()`: `listener.accept()` (both the `SO_TIMEOUT` busy-poll branch and the
        unbounded branch) was never bracketed in `begin_blocking_region`/`end_blocking_region`.
      - `socket_connect()`: worse than just missing the STW bracket — `connect()`/`connect_timeout()`
        ran *inside* `with_socket()`'s closure, which holds the single global socket-registry
        write lock for the call's duration, serializing every other blocking `Socket` op
        process-wide for as long as the connect takes.
    Applied the same fix defensively to `native-io/src/socket_channel.rs`'s blocking-mode
    `SocketChannel` paths (`sc_connect_inner`'s `allow_block` branch, `sc_read`/`sc_write`), which
    match the identical pattern but were not directly confirmed as hit by these connectors (all
    3 remaining connectors configure their channels non-blocking).
    **However, direct gdb evidence shows this missing-wrap pattern is NOT what actually causes the
    residual hangs.** Every reproduced hang (both pre- and post-fix, including from the real
    `ClientHttpConnectorTests` class itself) showed: zero threads parked in an unwrapped blocking
    `accept`/`connect`/`read`/`write` syscall; zero `"STW cross-thread JIT takeover is still
    waiting for cooperative mutators"` warnings; and thread counts that *dropped* between
    successive snapshots 4s apart (proving forward progress, not a permanent deadlock). What IS
    reproducibly visible at every capture: one thread executing `vm/src/runtime/interpreter.rs`'s
    JIT-to-interpreter transition (`jit_invoke_virtual_mic` → `invoke_on_class_shared_inner` →
    `execute()` at `interpreter.rs:4163`) deep-cloning a method's `CodeAttribute` — specifically its
    `LineNumberEntry`/`LocalVariableEntry` vectors (`reader/src/attribute.rs::clone()`) — taking
    multiple seconds, in one capture while another thread waited on a `ConcurrentHashMap` per-bin
    monitor (`native_chm_compute` → `monitor_enter_gc_safe`, itself correctly GC-safe) presumably
    held by a thread doing the same slow clone. This looks like severe, non-deterministic
    interpreter/attribute-cloning + lock-contention slowness under the heavy thread-pool
    accumulation the 45-sub-test class produces in one process (76+ live threads by sub-test 6),
    not a deadlock — StepVerifier's wait just outlasts whatever timeout the harness enforces.
    **Verification**: `cargo test -p cratonvm-native-builtins --lib` 2996/0 and
    `cargo test -p cratonvm-native-io --lib` 349/0 unchanged (no regression). Stress comparison
    of the fix vs. pre-fix binary was inconclusive/confounded (both showed hangs at broadly
    similar rates under concurrent-load conditions on the shared build host) — the fix is landed
    because it closes a real, verified bug of the exact hypothesized pattern, not because it was
    confirmed to eliminate this residual. **Still OPEN**: the interpreter/attribute-cloning
    slowness above is the real next step, flagged separately for a dedicated investigation (not a
    quick missing-wrap fix — needs profiling why `CodeAttribute` line-number/local-variable data is
    deep-cloned per invocation instead of shared/cached).
    Also unfixed: the T19.H1 watchdog stack-dump itself SIGSEGVs when JIT frames are on the stack
    (separate small bug; `--nojit` dumps work).
*   ~~`web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` —
    pathological slowness (>1800s vs HotSpot's 13s, 160 tests).~~ **2026-07-15 update**: confirmed
    genuine forward progress, not a hang (frame counts change across successive
    `--stack-dump-on-timeout` watchdog dumps — the watchdog fires repeatedly during a single run,
    which doubles as a free sampling profiler: 10,970 dumps captured over ~40s). Aggregating the
    innermost non-framework frame across all dumps found the hot path: **`ConfigurationClassParser.
    parse`** (6004 samples), **`AbstractHttpHandlerIntegrationTests.startServer`** (5970), and —
    disproportionately — raw **Xerces XML parsing** (`XML11Configuration.parse` 4638 samples,
    `XMLDTDValidator.emptyElement` 1986, full SAX/DTD-scanning call chain beneath it) — roughly
    **42% of all sampled CPU time** inside Xerces, for a workload (annotation-`@Configuration`
    Spring context + embedded Tomcat/Reactor/Jetty bootstrap, 160x) that should barely touch XML
    parsing at all.

    **ROOT-CAUSED AND FIXED (2026-07-15, commits `4290124b` + `96c8a57f`, branch
    `fix/xerces-perf-20260715`, merged to `dev`)**. This turned out to be a GENERAL bug, not
    Xerces-specific — confirmed via a standalone `SAXParserFactory` parse-loop repro
    (`XercesRepro.java`, no Spring/Tomcat/networking involved) that isolated the exact same
    disproportionate cost outside any Spring context. Root cause, found by instrumenting the
    invoke-dispatch path with a one-off counter (`CRATONVM_DBG_INVOKESTATS`, kept in the tree as a
    permanent gated diagnostic): `execute_frame`'s raw-byte-peek fast dispatch loop — which includes
    the monomorphic `InvokeCache` inline cache used by `invokevirtual`/`invokespecial`/
    `invokeinterface` — is gated off for `is_jdk_class` frames (`vm/src/runtime/interpreter.rs`
    around line 6813; a deliberate, correct gate — some of that loop's *other* opcode fusions use
    truly-unchecked stack pops unsafe for real-JDK bytecode shapes). But the general
    `Instruction::decode` → `execute_instruction` path those frames fall back to called
    `execute_invoke`/`execute_invoke_kind` **unconditionally** for every invoke instruction — it
    never consulted the cache at all. Since Xerces (`com.sun.org.apache.xerces.internal.*`) ships
    inside `java.xml`, every single method call it makes — and Xerces's SAX/DTD state-machine scanner
    makes an enormous number of very small calls per parse — paid full method resolution (native-
    registry hash lookup, hierarchy walk, `class_manager` RwLock reads, several hardcoded special-case
    string comparisons) instead of the O(1) lock-free cache hit non-JDK bytecode already got.
    Measured with `CRATONVM_DBG_INVOKESTATS` on the repro: ~1,546 cache hits vs ~1,800,000 full-slow-
    path calls over 20 parses (a ~0.08% hit rate) before the fix. Fix: the `Instruction::Invokevirtual`
    / `Invokespecial` / `Invokeinterface` arms in `execute_instruction`
    (`vm/src/runtime/interpreter.rs`, "Method invocation (slow path)") now call
    `execute_invokevirtual_cached` first and only fall through to the existing slow path on a genuine
    cache miss — safe to reuse unconditionally because that function's arg decode already goes through
    `pop_arg_for_descriptor_checked` (descriptor-aware, checked), not the unchecked pops the
    `is_jdk_class` gate exists to avoid. A companion investigation (commit `96c8a57f`) found and fixed
    two related JIT-eligibility-check memoization gaps in the same hot path (a full O(bytecode-size)
    `jit_method_calls_native_shadowed` re-scan that never got cached via the existing
    `mark_jit_bail_listed`/`jit_skip_set` mechanisms, so it re-ran every 64 invocations forever for any
    method that calls a native-shadowed target).

    **Verification — isolated repro** (`XercesRepro.java`, 500 SAX parses of an ~800-line
    web.xml-shaped document, `dtds/web-app_2_3.dtd` for the validating case):

    | Mode | Before | After | HotSpot | Speedup |
    |---|--:|--:|--:|--:|
    | non-validating | 155.5 ms/parse | 57.2 ms/parse | 0.46 ms/parse | 2.72x |
    | DTD-validating | 536.8 ms/parse | 156.3 ms/parse | 1.18 ms/parse | 3.44x |

    `perf record` self-time for `NativeMethodRegistry::find` dropped 11.61%→4.86% and
    `jit_method_calls_native_shadowed` dropped 5.18%→0.58% across the two fixes.
    `cargo test -p cratonvm-vm --lib`: 2203 passed / 9 failed (all 9 are pre-existing
    `runtime::lock_order` tests that explicitly require a debug build — "test runner is expected to
    be a debug build" — an artifact of running `cargo test --release`, unrelated to this change) /
    111 ignored.

    **Full-class wall-clock impact: NOT confirmed, OPEN follow-up**. A same-day A/B run of the full
    160-test class (fixed vs. pre-fix binary, bounded ~10-minute windows, same shared build host) was
    inconclusive: the fixed binary averaged ~49.7s/test vs. ~40.9s/test pre-fix in that particular
    window — but the host had a load average of 5-6 on 16 cores with a *different concurrent
    session's* Spring suite run consuming 169% CPU at the time, so this specific comparison is not
    trustworthy in either direction and should not be read as "the fix made this test slower." More
    significant than the noise: per-test wall time in both runs was ~40-55 SECONDS — two to three
    orders of magnitude larger than what the isolated repro's few-hundred-ms Xerces parse cost could
    plausibly explain per test — meaning this specific integration test's dominant per-test cost is
    something else entirely (each test builds a fresh embedded Tomcat/Reactor instance: real socket
    bind, NIO connector startup, thread-pool bootstrap), not the CPU-bound bytecode-dispatch overhead
    this fix targets. **This means the fix is real, general, and verified, but is not sufficient on
    its own to bring `RequestMappingMessageConversionIntegrationTests` down to a small multiple of
    HotSpot's 13s.** The next step for that specific goal is a separate investigation into what
    dominates per-test wall-clock time in this class — likely embedded-server bootstrap/socket/
    thread costs — ideally run on an uncontended host to get a clean baseline.
*   `web.reactive.result.view.script.JRubyScriptTemplateTests` — JRuby-on-CratonVM: JRuby's own
    bundled `rubygems/specification.rb` bootstrap fails with a Ruby-level `SyntaxError` from code it
    generates itself: `#{@@nil_attributes.map {|key| "@#{key} = nil" }.join "; "}` (a Ruby
    string interpolation reading a `.map {|key| ...}` block parameter, inside JRuby's own
    precompiled-to-JVM-bytecode stdlib) interpolates `key` as EMPTY instead of the Symbol's name,
    producing malformed generated Ruby source (`"@ = nil; @ = nil; ..."`) that a second, inner
    `Kernel#eval` then rejects. **2026-07-15 update**: minimal standalone repro isolated (no Spring
    needed — `ScriptEngineManager().getEngineByName("jruby").eval(...)` alone triggers it during
    JRuby's own lazy bootstrap, before any user script runs); confirmed CratonVM-specific
    (`Symbol#to_s` and simple top-level `"#{key}"` interpolation both work correctly in isolation —
    the bug is specific to a block-parameter interpolated inside JRuby's own PRE-COMPILED bytecode,
    not JRuby's general interpolation mechanism). Found a concrete, reproducible clue: 5
    `[GC-ARRAY-GUARD] array_length(non-array)` warnings fire (`class_id=1013` in one run,
    consistently 5 of them — matching `@@nil_attributes`' likely element count) at the EXACT moment
    the interpolation corrupts, from `gc/src/gen_heap.rs:2314`'s defensive guard (a raw JVM
    `arraylength` bytecode instruction executing against a heap object CratonVM's GC does NOT
    consider an array — silently returns 0 instead of crashing). `CRATONVM_DBG_STALE_OBJREF=1`
    was tried but did NOT visibly fire for this repro (inconclusive either way — this assertion has
    known coverage gaps for other bug families, per `stream-arraylist-gc-pressure-heap-corruption-
    found-20260714.md`). **Leading hypothesis, not confirmed**: a stale/wrong `ObjectRef` — the
    JVM bytecode JRuby's own compiler emitted for this interpolation legitimately expects an array
    (its own internal representation of the block-parameter/interpolation-piece list), but by the
    time the `arraylength` instruction executes, the reference has been relocated/reused to point at
    a non-array object — matching the broader stale-ObjectRef bug family already extensively
    tracked in this codebase (see `wildfly-parallel-boot-stale-objectref-residual.md`,
    `stale-objectref-static-sweep-20260711.md`) but not yet localized to a specific call site here.
    **Next step**: reproduce under `RUST_BACKTRACE=1` + the `CRATONVM_GC_ARRAY_GUARD_BT=1` backtrace
    (already captured once — the backtrace bottoms out in the raw interpreter `arraylength` opcode
    handler, `interpreter.rs:8638`, giving no further attribution on its own) combined with a
    JRuby-side decompile of the exact bytecode `specification.rb`'s `set_nil_attributes_to_nil`
    heredoc-eval compiles to (JRuby ships this stdlib file pre-compiled; extract the `.class`
    equivalent from the `jruby-stdlib` jar with `javap` to see the literal `arraylength`
    instruction's context) to identify which allocation/GC event upstream could relocate the
    reference this instruction reads. Not reactive-specific; a fix here likely benefits any JRuby
    (or generally: any dynamic-bytecode-generating library that emits `arraylength`) workload.
*   ~~Batch-context `<clinit>` contamination~~ — RETRACTED, see above (host environment issue: missing /tmp + missing ~/jdk25 symlink, not CratonVM).
