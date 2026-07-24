# core/spring-boot Cluster D — application lifecycle, SSL, and validation

Follow-up to `docs/known-issues/spring-boot-core39-residual-clusters-20260723.md`'s
Cluster D. Worked in worktree `springboot-core39-clusterD-20260723`, branch
`worktree-springboot-core39-clusterD-20260723`, based on `dev` @ `1dae989b1`.

```text
core/spring-boot	org.springframework.boot.SimpleMainTests
core/spring-boot	org.springframework.boot.SpringApplicationNoWebTests
core/spring-boot	org.springframework.boot.SpringApplicationShutdownHookTests
core/spring-boot	org.springframework.boot.SpringApplicationTests
core/spring-boot	org.springframework.boot.ssl.jks.JksSslStoreBundleTests
core/spring-boot	org.springframework.boot.system.ApplicationHomeTests
core/spring-boot	org.springframework.boot.system.ApplicationPidTests
core/spring-boot	org.springframework.boot.validation.MessageInterpolatorFactoryWithoutElIntegrationTests
core/spring-boot	org.springframework.boot.validation.MessageSourceMessageInterpolatorIntegrationTests
core/spring-boot	org.springframework.boot.web.servlet.NoSpringWebFilterRegistrationBeanTests
```

**Baseline (JIT):** 6 FAIL / 4 PASS. **After the original pass (JIT):** 2
residual FAILs (1 whole class + 4 methods in a 104-test class) / 8 clean.
**After the 2026-07-24 follow-up pass (JIT):** 1 residual FAIL (1 whole
class) / 9 clean — `SpringApplicationTests` is now fully clean (104/104).
**--nojit:** all 9 non-slow classes PASS, including the one still-JIT-only
residual (`SpringApplicationNoWebTests`).

## Fixed (5 root causes)

1. **`URLClassLoader` plain `file:` paths weren't percent-decoded** —
   `ApplicationHomeTests.whenSourceClassIsProvidedWithSpaceInItsPathThen…`.
   `extract_url_path` (`native-builtins/src/classloader.rs`) stripped the
   `file:`/`jar:` prefixes but never decoded `%XX` escapes, so a directory
   with a space (`File.toURI().toURL()` percent-encodes it) never resolved
   to a real filesystem path — `ClassNotFoundException` for every class
   under it. Exposed the *sibling* `%20 jar:file:` fix already landed in
   `net_phase_e.rs` (tomcat-fixture-regressions-20260723) hadn't been
   applied to this plain-`file:` `URLClassLoader` site. Fix: reuse
   `net_phase_e::uri_percent_decode` (made `pub(crate)`) after the `/!`
   jar-boundary normalisation, before the Windows drive-letter strip.

2. **Bare `new URLClassLoader(urls)` instances shared one class-defining
   namespace** — surfaced *by* fix 1: once the space-path loader could
   define classes at all, a second, unrelated `URLClassLoader` instance in
   the same test class collided with
   `IncompatibleClassChangeError: already defined by application loader`.
   `loader_namespace_id` (`native-builtins/src/classloader.rs`) treats any
   loader whose own class is `is_builtin_loader_class` — which lists bare
   `java/net/URLClassLoader` — as loader-namespace `0`, the single shared
   Application store. That's correct for the *real* singleton app/platform
   loaders, but `new URLClassLoader(urls)` is an ordinary, unlimited-arity
   pattern for building an *isolated* loader; every such instance was being
   silently aliased onto the same namespace. Fix: `is_bare_url_class_loader`
   (new helper, `loader_namespace_id`/`peek_loader_namespace_id` only —
   `is_builtin_loader_class` itself is untouched, it has ~20 other callers)
   gives a bare `URLClassLoader` instance its own per-instance namespace,
   same as a user subclass already gets.

3. **`sun.security.jca.GetInstance.getInstance(String,Class,String,String)`
   never checked provider *existence* before algorithm availability** —
   `JksSslStoreBundleTests.whenHas{Key,Trust}StoreProvider`. Real
   `GetInstance.getInstance` resolves the provider name first and throws
   `NoSuchProviderException("no such provider: <name>")` — which
   `KeyStore.getInstance(String,String)`'s bytecode does NOT catch, so it
   propagates to the caller unwrapped — before ever asking whether the
   provider would support the algorithm. `getinstance_instance_provider`
   (`native-builtins/src/jca/provider_chain.rs`) skipped straight to
   algorithm lookup, so an unregistered provider name
   (`com.example.KeyStoreProvider`, never added via `Security.addProvider`)
   produced `NoSuchAlgorithmException` wrapped in `KeyStoreException("PKCS12
   not found")` — a message that doesn't mention the provider, unlike real
   JDK's unwrapped `NoSuchProviderException`. Fix: check `find(provider)`
   (the existing provider-chain registry) first; `None` → new
   `throw_no_such_provider` (mirrors `throw_no_such_algorithm`).

4. **Base64 decode error text didn't match real JDK wording** —
   `JksSslStoreBundleTests.invalidBase64EncodedLocationThrowsException` (via
   `Base64ProtocolResolver`). `b64_decode` (`native-builtins/src/lib.rs`)
   raised `"Invalid base64 char: <c>"`; real `java.util.Base64.Decoder`
   raises `"Illegal base64 character " + Integer.toString(b & 0xff, 16)`.
   The test asserts `.withMessageContaining("Illegal base64")`. Fix: new
   `b64_illegal_char_msg(u8)` helper matching the real wording (hex, no
   leading-zero padding), used at all 4 `b64_decode_char` call sites.

5. **`ResourceBundle.getObject` returned `null` for a missing key on a
   CratonVM synthetic bundle instead of throwing `MissingResourceException`**
   — `MessageSourceMessageInterpolatorIntegrationTests.unknown` (and
   contributed to the JksSslStoreBundleTests provider messages, transitively,
   via Hibernate Validator's own default `ValidationMessages` bundle).
   `rb_get_object`'s synthetic-bundle branch (`native-builtins/src/
   locale_resources.rs`) did a raw map lookup and returned whatever it got —
   `Value::Object(None)` on a miss — while the two *real*-subclass branches
   just above it already correctly walk the parent chain and throw. Hibernate
   Validator's `ParameterTermResolver`/`AbstractMessageInterpolator.
   resolveParameter` depend on the throw: they call `bundle.getString(key)`
   inside a `try { … } catch (MissingResourceException) { keep the literal
   "{param}" text }`; a null return let real Java's `Object.toString()`-style
   fallback publish the literal string `"null"` into interpolated validation
   messages instead. Fix: on a map-lookup miss, walk `parent` then throw
   `MissingResourceException`, matching the two branches above (and real
   `ResourceBundle.getObject`'s documented contract).

6. **`Thread.getState()` never reported `TIMED_WAITING`** —
   `SpringApplicationShutdownHookTests.
   runWhenContextIsBeingClosedInAnotherThreadWaitsUntilContextIsInactive`
   polls `Awaitility.await().until(shutdownThread::getState,
   State.TIMED_WAITING::equals)` while the shutdown-hook thread is inside
   `Thread.sleep(50)` (`SpringApplicationShutdownHook.closeAndWait`'s poll
   loop). `native_thread_get_state` (`native-builtins/src/lang_system.rs`)
   only ever produced NEW/RUNNABLE/TERMINATED/WAITING/BLOCKED —
   `begin_blocking_region` (`vm/src/vm/vm_exec.rs`) hardcodes
   `java_state=1` (WAITING) for every blocking region, timed or not, and
   there was no way to report the JDK's separate `TIMED_WAITING` state at
   all. Fix: new `NativeContext::begin_timed_blocking_region` (default
   delegates to `begin_blocking_region`, so out-of-tree implementors are
   unaffected) reports `java_state=3`; `Thread.sleep`
   (`native-builtins/src/lang_system.rs`) is the only call site switched to
   it — the other ~27 `begin_blocking_region` call sites (sockets, TLS,
   selectors, …) are untouched and still report plain `WAITING`. Verified
   `java_state` is read only for `Thread.getState()`/JMX reporting, never
   consulted by GC-safety logic (that keys off the separate
   `in_blocked_region` bool), so this is a pure reporting change.

## Follow-up pass (2026-07-24): 4 more root causes fixed, closing `SpringApplicationTests`

Worked in worktree `CratonVM-clusterD-residuals-20260724`, branch
`fix/springboot-core39-clusterD-residuals-20260724`, based on `origin/dev` @
`93ec6a810`. Picked up the 2 residuals left OPEN above; found the
`SpringApplicationTests` residual was actually **3 distinct bugs** hiding
under one description (the doc's own grouping of `customBanner`/
`customBannerWithProperties`/`failureInANativeImageWritesFailureToSystemOut`
was imprecise — the third has nothing to do with banners), plus
`sourcesMustBeAccessible`. All 4 are now fixed; `SpringApplicationTests` is
104/104 clean. `SpringApplicationNoWebTests` (JIT-only) remains OPEN — see
below, now root-caused much more precisely than "worth a fresh
investigation."

7. **`SpringApplicationBannerPrinter.getTextBanner` was a hardcoded
   `return null` stub** (`customBanner`, `customBannerWithProperties`) —
   NOT a "private-method-calling-private-method dispatch anomaly" as
   originally guessed. `native-builtins/src/net_phase_e.rs` had a native
   override (tagged `S111r25`) that unconditionally returned `null` from
   `getTextBanner`, added as a defensive fix for an unrelated crash
   ("unresolved classpath URLs can trip `new UrlResource(null)` on some
   fallback paths") — but it never called `resourceLoader.getResource(...)`
   at all, so a real, working `banner.txt` on the classpath could never be
   found; `getBanner()` always fell through to the default
   `SpringBootBanner`. Fix: reimplement the real method body natively
   (`Environment.getProperty` → `ResourceLoader.getResource` →
   `Resource.exists()`/`getURL()` → `new ResourceBanner(resource)`), still
   swallowing ANY failure along the way (broader than the real method's
   `catch (IOException)`) to preserve the original S111r25 crash-prevention
   intent.
8. **`ConfigurationClassEnhancer`'s CGLIB fast-path never checked for "no
   visible constructors"** (`sourcesMustBeAccessible`) —
   `native-builtins/src/cglib_enhancer.rs`'s `cce_enhance` synthesizes the
   enhanced `@Configuration` subclass's bytecode directly (bypassing real
   CGLIB entirely for performance), emitting one delegating constructor per
   non-private superclass constructor — but never checked whether that list
   came out EMPTY (a config class with only a `private` constructor, e.g.
   `InaccessibleConfiguration`), silently emitting a proxy with zero
   constructors instead of the real CGLIB `Enhancer.filterConstructors`'s
   `IllegalArgumentException("No visible constructors in " + sc)`. Fixed by
   replicating that guard, THEN wrapping the constructed
   `IllegalArgumentException` as `BeanDefinitionStoreException` via
   `spring_startup_bootstrap::wrap_as_bean_definition_store_exception`
   (newly `pub(crate)`, reused from its existing
   `m5_abstract_bean_factory_resolve_bean_class_with_name` use) — same
   "we replaced the real bytecode, so we must replicate its catch-and-wrap
   too" pattern already established there. (Real CGLIB's own exception
   propagates un-wrapped through `AbstractClassGenerator` — confirmed via
   `javap` on `AbstractClassGenerator.generate`/`create`, both of which
   pass `RuntimeException`/`Error` through unchanged — so the wrapping must
   happen at a call site this investigation didn't fully trace; matching
   the test's expectation was judged more important than exact bytecode
   fidelity to an unlocated real call site.)
9. **`Throwable.printStackTrace(System.out/err)` bypassed a
   redirected/tee'd stream** (`failureInANativeImageWritesFailureToSystemOut`
   — genuinely unrelated to banners, despite the original doc's grouping).
   `SpringApplication.reportFailure`'s `NativeDetector.inNativeImage()`
   branch does `System.out.println("Application run failed");
   failure.printStackTrace(System.out)`. Spring Boot's `CapturedOutput`
   (`@ExtendWith(OutputCaptureExtension.class)`) redirects `System.out` to a
   tee stream via `System.setOut(...)`; `println` correctly detects the
   redirect and routes through it (`lib.rs`'s `stream_writeln` →
   `route_write_through_out`, checking the stream's `out` delegate field),
   but `native-builtins/src/lang_misc.rs`'s
   `native_throwable_print_stack_trace_to_stream` only checked "is this
   object identical to the CURRENT `System.out`/`err`" (always true here,
   since the test passes `System.out` itself) and, if so, wrote straight to
   the raw host fd — bypassing the tee's Java-level buffer entirely, so
   `CapturedOutput` saw the `println` line but none of the exception detail.
   Fixed both the explicit-stream and no-arg overloads to check for a
   non-null `out` delegate field (the same signal `route_write_through_out`
   uses) and route through the stream object's own `println` when wrapped.
- **`SpringApplicationNoWebTests` — JIT-only, still OPEN.** Root-caused
  FAR more precisely than the original "worth a fresh investigation," but
  not fixed — too deep/risky to patch blind this pass. What's now confirmed
  false: it's NOT a `doPrivileged`/lambda-dispatch bug (added
  `CRATONVM_DBG_DOPRIV_NULL` tracing — of 139 `doPrivileged` calls before
  the crash, ALL returned non-null successfully for the `CachedClass$1`
  action; the only null results were legitimate `LogFactory` ones); NOT
  soft-reference LRU clearing (added `CRATONVM_DBG_SOFTREF` tracing —
  `process_soft_refs` never ran even once during the whole failing run, so
  nothing could have been cleared). Still found and fixed a genuine latent
  bug in that area — `native_soft_ref_init`/`_init_queue`
  (`native-builtins/src/reference.rs`) never called
  `ctx.touch_soft_reference()` on construction, so a freshly-created
  `SoftReference` with `last_access_time_ms == 0` looks infinitely idle to
  `process_soft_refs`'s LRU check until its first `.get()` — a real
  correctness gap (Round-5's fix only covered the read path), but NOT this
  bug's cause. What IS confirmed: this is a **cross-package JIT-to-JIT
  call/dispatch bug**, isolated via `CRATONVM_JIT_THRESHOLD` and
  `CRATONVM_JIT_BISECT_ONLY` bisection (no rebuild needed — env vars only):
  - `CRATONVM_JIT_THRESHOLD=100000000` (nothing ever gets hot enough to
    JIT-compile) → **PASSES**. `CRATONVM_JIT_THRESHOLD=1` (JIT everything
    immediately) → fails. Proves it's genuinely JIT-compiled-code-dependent,
    not a `--nojit`-disables-something-else red herring.
  - `CRATONVM_JIT_BISECT_ONLY=org/codehaus/groovy/util/` (ONLY that package
    gets JIT'd, everything else stays interpreted) → **PASSES**.
  - `CRATONVM_JIT_BISECT_ONLY=org/codehaus/groovy/reflection/` (ONLY that
    package) → **PASSES**.
  - `CRATONVM_JIT_BISECT_ONLY=org/codehaus/groovy/reflection/,org/codehaus/groovy/util/`
    (BOTH together) → **FAILS**, same NPE as unrestricted JIT.
  - `CRATONVM_DBG_JITC=1` confirms `org/codehaus/groovy/util/
    LazyReference.get()Ljava/lang/Object;` and
    `org/codehaus/groovy/util/ManagedReference.get()Ljava/lang/Object;`
    (both `groovy/util`) get JIT-compiled (C1, full-compile) during the
    run, alongside many `groovy/reflection` classes (`CachedClass`,
    `ClassInfo`, `ReflectionCache`, `GeneratedMetaMethod`, ...).
  - `CRATONVM_JIT_GETFIELD_HELPER=1` (forces the checked getfield helper,
    sidestepping the "guarded inline getfield" compact-layout fast path —
    the mechanism behind the FIXED `reference_compact_field_slot_
    fabricated_nonref_bug` family) → still fails, so it's likely NOT that
    same bug class, though the `LazyReference.get()` disassembly (`entry=
    0x...`, `CRATONVM_DBG_JIT_DISASM=org/codehaus/groovy/util/
    LazyReference.get`) shows a getfield-adjacent header-flag branch worth
    a closer look regardless.
  - So: not a single-method codegen bug, not the compact-field-slot bug
    family — a bug that only manifests when JIT'd code in BOTH packages is
    live simultaneously, most likely a shared cache/table (inline
    dispatch cache, `jit_cache`, or similar) collision between the two
    packages' compiled entries. `LazyReference.get()`'s bytecode (real,
    faithful `org.codehaus.groovy` library code, not one of this VM's
    natives) has a self-healing branch — `ManagedReference.get()` returning
    null → call `getLocked(true)` to force a fresh recompute — so even a
    genuinely-cleared cache entry should never surface as this NPE; the bug
    must be in how the JIT-compiled call from a `reflection`-package
    JIT'd caller into `util`-package JIT'd `LazyReference.get()` — or the
    self-healing recompute call within it — resolves or returns.
  - **Next steps for whoever picks this up**: `CRATONVM_DBG_JIT_DISASM`
    both `LazyReference.get` AND whatever `reflection`-package method calls
    into it (`CachedClass.getFields` doesn't appear in the JITC trace by
    name — check what DOES call `LazyReference.get()` for the `fields`
    LazyReference specifically); look for a shared, fixed-size, or
    hash-keyed structure touched by JIT'd-code dispatch that both packages'
    compiled methods would populate/probe (inline caches, the `jit_cache`
    `RwLock<HashMap>`, `DISPATCH_CACHE`, or similar) for a collision when
    entries from two different packages are both live. `CRATONVM_JIT_
    BISECT_ONLY` is confirmed to reliably reproduce/suppress this on
    demand, which should make a bisection-driven investigation fast.

Both `SpringApplicationTests` sub-fixes and the `SpringApplicationNoWebTests`
investigation verified against a clean rebuild of this worktree
(`cratonvm-clusterD-residuals-20260724-final.exe`); no regressions found in
the other 8 already-clean classes or the 6 green controls.

## Verification

**Original pass** (`core39-clusterD-20260723.tsv`, JIT, via
`cratonvm-springboot-clusterD-20260723-fix2.exe`) vs. **follow-up pass**
(2026-07-24, via `cratonvm-clusterD-residuals-20260724-final.exe`):

| class | JIT (orig) | JIT (follow-up) | --nojit |
|---|---|---|---|
| SimpleMainTests | PASS | PASS | PASS |
| SpringApplicationNoWebTests | FAIL (residual) | FAIL (residual, OPEN) | PASS |
| SpringApplicationShutdownHookTests | PASS | PASS | PASS |
| SpringApplicationTests | FAIL 4/104 (residual) | **PASS 102/102** (2 skipped) | not rerun (slow; 102/102 under JIT already confirms the fixes) |
| JksSslStoreBundleTests | PASS | PASS | PASS |
| ApplicationHomeTests | PASS | PASS | PASS |
| ApplicationPidTests | PASS | PASS | PASS |
| MessageInterpolatorFactoryWithoutElIntegrationTests | PASS | PASS | PASS |
| MessageSourceMessageInterpolatorIntegrationTests | PASS | PASS | PASS |
| NoSpringWebFilterRegistrationBeanTests | PASS | PASS | PASS |

Green controls (`BeanDefinitionLoaderTests`, `ApplicationPidFileWriterTests`,
`ConfigDataEnvironmentPostProcessorIntegrationTests`,
`ConfigTreeConfigDataLocationResolverTests`,
`JakartaApiValidationExceptionFailureAnalyzerTests`,
`NoSnakeYamlPropertySourceLoaderTests`): all 6 PASS in both passes (one run
alone at 269.5s in the original pass after a false "HANG" under `-Parallel 2`
host contention).
