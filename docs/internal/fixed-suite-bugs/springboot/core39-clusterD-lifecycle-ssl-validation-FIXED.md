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

**Baseline (JIT):** 6 FAIL / 4 PASS. **After fixes (JIT):** 2 residual FAILs
(1 whole class + 4 methods in a 104-test class) / 8 clean. **--nojit:** all 9
non-slow classes PASS, including the one JIT-only residual.

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

## OPEN residuals (not fixed this pass)

- **`SpringApplicationNoWebTests` — JIT-only.** PASSES under `--nojit`;
  under JIT, `GroovySystem.<clinit>` → `MetaClassRegistryImpl.<init>` →
  `MetaClassImpl.reinitialize` → `inheritStaticInterfaceFields` →
  `addFields` NPEs on `CachedClass.getFields()` returning `null` for some
  interface's static-field metaclass setup
  (`groovy.lang.MetaClassImpl.addFields:2504`,
  `"Cannot read the array length because \"<local2>\" is null"`). This is
  Groovy bootstrap, not script compilation, so it's likely NOT the same
  family as the already-deep-dived "Groovy compiler internals" ANTLR/dup2
  residuals in `docs/internal/fixed-suite-bugs/springboot/spring-bug-11-
  groovy-and-scheduler-crashes.md` — worth a fresh, narrower investigation
  (start from `CachedClass.getFields()`'s `LazyReference`/`ManagedReference`
  chain and whether the register-resident-missed-root residual documented in
  `docs/internal/fixed-suite-bugs/wildfly/bug-06b-jit-scan-cache-unsound.md`
  reaches this specific reflection-mirror-array builder under JIT).
- **`SpringApplicationTests` — 4 of 104 methods.** `customBanner`,
  `customBannerWithProperties`, `failureInANativeImageWritesFailureToSystemOut`
  all print the DEFAULT `SpringBootBanner` instead of a `banner.txt`
  resource's `ResourceBanner`. Root-caused to `SpringApplication.
  printBanner`/`SpringApplicationBannerPrinter.getTextBanner`: a **direct**
  call — `resourceLoader.getResource("banner.txt").exists()` — from
  standalone repro code correctly finds a custom classloader's `banner.txt`
  on this VM (byte-identical to HotSpot), but the SAME lookup, made from
  *inside* `SpringApplicationBannerPrinter.getTextBanner`'s own bytecode
  (reached via `getBanner()`, invoked reflectively or through
  `SpringApplication.run()`), never calls the classloader's `getResource`
  at all and silently falls through to the default banner — with the
  `resourceLoader` field on the printer instance confirmed (via reflection)
  to be the exact same, working `DefaultResourceLoader`. Reproduces with
  plain `SpringApplication.run()` (no Mockito `spy()`, no `@WithResource`
  extension) via a minimal repro
  (`new SpringApplication(Config.class).run()` with a custom
  `ClassLoader.getResource` override as TCCL). Root cause not yet pinned —
  looks like a private-method-calling-private-method dispatch anomaly
  specific to this call shape, not a resource-loading bug per se (the same
  resource mechanism works when called directly). `sourcesMustBeAccessible`
  is unrelated: `new SpringApplication(InaccessibleConfiguration.class).run()`
  should throw `BeanDefinitionStoreException` (root cause
  `IllegalArgumentException: "No visible constructors"`) but doesn't throw
  at all — a separate, not-yet-investigated constructor-visibility gap in
  reflective bean instantiation.
- Both residuals verified to be **pre-existing**, not introduced by this
  pass's fixes: `SpringApplicationNoWebTests` fails identically before/after
  under JIT (passes both before/after under `--nojit`);
  `SpringApplicationTests` shows the identical 4-method failure set
  before/after (98/104 both times). Its "HANG at 300s" under suite-runner
  load is confirmed host-contention (this box runs many concurrent
  worktree builds/suites) — run alone with `-TimeoutSec 900` it completes
  in ~270-370s with the same 4 failures, not a deadlock.

## Verification

`core39-clusterD-20260723.tsv` (JIT) + the same list minus
`SpringApplicationTests` (`--nojit`) + the 6-class green-controls list, all
via `cratonvm-springboot-clusterD-20260723-fix2.exe`:

| class | JIT | --nojit |
|---|---|---|
| SimpleMainTests | PASS | PASS |
| SpringApplicationNoWebTests | FAIL (residual) | PASS |
| SpringApplicationShutdownHookTests | PASS | PASS |
| SpringApplicationTests | FAIL 4/104 (residual) | not rerun (slow) |
| JksSslStoreBundleTests | PASS | PASS |
| ApplicationHomeTests | PASS | PASS |
| ApplicationPidTests | PASS | PASS |
| MessageInterpolatorFactoryWithoutElIntegrationTests | PASS | PASS |
| MessageSourceMessageInterpolatorIntegrationTests | PASS | PASS |
| NoSpringWebFilterRegistrationBeanTests | PASS | PASS |

Green controls (`BeanDefinitionLoaderTests`, `ApplicationPidFileWriterTests`,
`ConfigDataEnvironmentPostProcessorIntegrationTests`,
`ConfigTreeConfigDataLocationResolverTests`,
`JakartaApiValidationExceptionFailureAnalyzerTests`,
`NoSnakeYamlPropertySourceLoaderTests`): all 6 PASS (one run alone at
269.5s after a false "HANG" under `-Parallel 2` host contention).
