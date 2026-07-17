# `ServletWebServerApplicationContext.getWebServerFactory()` native Tomcat shim crashes non-Tomcat (Jetty/generic) test modules — `TomcatServletWebServerFactory` class-not-found process abort

**Status: OPEN**
**Severity: HIGH** (10 fatal process CRASHes across two Gradle modules whose real classpath never contains the referenced class)

| | |
|---|---|
| **Modules** | `module/spring-boot-web-server`, `module/spring-boot-jetty` |
| **Classes (10)** | `module/spring-boot-web-server`: `MissingWebServerFactoryBeanFailureAnalyzerTests`, `AnnotationConfigServletWebServerApplicationContextTests`, `ServletComponentScanIntegrationTests`, `ServletWebServerApplicationContextTests`, `XmlServletWebServerApplicationContextTests`, `SpringApplicationWebServerTests`; `module/spring-boot-jetty`: `AutoConfigureWebServerJettyServletTests`, `JettyMetricsAutoConfigurationTests`, `JettyServletWebServerAutoConfigurationTests`, `JettyServletWebServerMvcIntegrationTests` |
| **Result** | fatal process `CRASH` (identical byte-for-byte error for all 10) |
| **Worktree** | `C:\craton\CratonVM-spring-boot-crashfail-20260714` (branch `feat/spring-boot-crashfail-20260714`) |
| **Logs** | `apps\spring-boot-suite-runner\.suite\results\rerun-20260716\shard{6,7}\logs\module_spring-boot-web-server....` / `module_spring-boot-jetty....` (`.out.log`/`.err.log` pairs, filenames truncated by the runner's log-naming scheme) |

## Symptom

Every one of the 10 classes ends its `.err.log` with the identical fatal line:

```
[cratonvm] main-vm run() returned Err: Error in thread "main" class file error: class not found: org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory
[cratonvm] main-vm run() Err (debug): Error in thread "main" class file error: class not found: org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory
```

For the Jetty test (`AutoConfigureWebServerJettyServletTests`), the `.out.log` shows a normal Spring Boot banner and the `.err.log` shows the application context actually starting — JUnit/Mockito/Jetty init, real CGLIB `@Configuration` enhancement of the test's own `ServletConfiguration` inner class:

```
[CCE] enhance: defined org/springframework/boot/jetty/autoconfigure/AutoConfigureWebServerJettyServletTests$ServletConfiguration$$EnhancerByCGLIB$$0 (super=...ServletConfiguration, marker=ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
[cratonvm] main-vm run() returned Err: Error in thread "main" class file error: class not found: org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory
```

— i.e. the crash happens deep into a real, legitimate context refresh for a Jetty-only application, not at classloading/bootstrap. There is no JUnit-level failure output; the whole process aborts before the test framework can report anything, which is why the runner classifies these as `CRASH` rather than a normal test `FAIL`.

## Analysis: this is NOT a legitimate cross-module test reference, and NOT a suite-runner classpath gap

I verified both possibilities named in this investigation's brief and ruled both out:

**1. The affected test source does not reference Tomcat at all.** Read the actual Spring Boot 4.1.0-SNAPSHOT sources under `C:\craton\CratonVM\apps\spring-boot`:
- `module/spring-boot-jetty/.../AutoConfigureWebServerJettyServletTests.java` — plain `@SpringBootTest` + `@AutoConfigureWebServer`, no Tomcat import.
- `module/spring-boot-web-server/.../MissingWebServerFactoryBeanFailureAnalyzerTests.java` — constructs a bare `ServletWebServerApplicationContext`/`ReactiveWebServerApplicationContext` and asserts on the *generic* `ServletWebServerFactory`/`ReactiveWebServerFactory` interface names; the production analyzer (`MissingWebServerFactoryBeanFailureAnalyzer.java`) only ever prints `beanType.getName()` generically — never `Tomcat*`.
- `grep -rl "TomcatServletWebServerFactory" --include=*.java .` across the whole spring-boot checkout returns zero hits inside `module/spring-boot-web-server` or `module/spring-boot-jetty` (production or test sources). Every real reference lives in `module/spring-boot-tomcat` itself or in Tomcat-specific integration/smoke-test modules.
- `JettyServletWebServerAutoConfiguration.java` (real source) is `@ConditionalOnClass({ServletRequest.class, Server.class, Loader.class, WebAppContext.class})` and `@Import(JettyWebServerConfiguration.class, ServletWebServerConfiguration.class)` — no Tomcat anywhere in the import graph. `ServletWebServerConfiguration.java` (the shared `spring-boot-web-server` base) also has no Tomcat reference.
- The module's own `AutoConfigureWebServer.imports` (the test-scope `@ImportAutoConfiguration` candidate file, `module/spring-boot-jetty/src/main/resources/META-INF/spring/org.springframework.boot.web.server.test.AutoConfigureWebServer.imports`) lists only `JettyReactiveWebServerAutoConfiguration` / `JettyServletWebServerAutoConfiguration` — no Tomcat auto-configuration is ever a candidate for this module.

**2. The suite-runner's classpath generation is correct and does exclude Tomcat's Spring module.** The runner (`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`, `Get-ModuleClasspathEntries`) uses the *actual* Gradle-resolved test runtime classpath, dumped per-module by a real `cratonvmTestCp` init-script task (`cratonvm-test-cp.init.gradle`) — not a hand-rolled or aggregated fat classpath. I inspected the generated classpath files directly:

```
module/spring-boot-web-server/build/cratonvm-test-cp.txt  → contains tomcat-embed-core-11.0.22.jar, tomcat-annotations-api-11.0.22.jar
module/spring-boot-jetty/build/cratonvm-test-cp.txt       → contains tomcat-embed-jasper/-core/-el, tomcat-annotations-api jars
```

Both modules' `build.gradle` do pull in raw **Apache Tomcat** jars (`org.apache.tomcat.embed:tomcat-embed-core` as a real `testImplementation` in `spring-boot-web-server`; `tomcat-embed-jasper` as an `optional` main dependency in `spring-boot-jetty`) — these satisfy `@ConditionalOnClass(Tomcat.class)`-style checks against the *Apache Tomcat* server library. But **neither classpath file contains any `spring-boot-tomcat` module jar or class directory** (`grep -i "spring-boot-tomcat"` on both files returns nothing) — confirmed by grep, not just build.gradle inspection. `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactory` is a **Spring Boot** class that only exists in `module/spring-boot-tomcat`'s compiled output, which genuinely is not, and per the real Gradle dependency graph should not be, on either module's classpath. Real HotSpot running these exact classes with this exact classpath would never load or need this class — there is no compiled `.class` file for it reachable at all.

**Conclusion: this is a genuine CratonVM-side bug**, not a test-authoring cross-reference and not a runner classpath-generation gap.

## Root cause

`native-builtins/src/net_phase_e.rs` (worktree HEAD `da808e3ec`, commit `3de9e929a` "Round 57: native `ServletWebServerApplicationContext.getWebServerFactory` shim", ~line 6727):

```rust
// S111r57 — bypass MissingWebServerFactoryBeanException by overriding
// ServletWebServerApplicationContext.getWebServerFactory() to allocate a
// TomcatServletWebServerFactory directly instead of asking the bean factory.
//
// In real Spring Boot, this protected method calls
//   getBeanFactory().getBeanNamesForType(ServletWebServerFactory.class)
// and throws MissingWebServerFactoryBeanException if zero matches. Under
// CratonVM the auto-configuration that registers the Tomcat factory bean
// never completes (Cglib/condition-evaluation issues upstream), so the
// lookup fails. We short-circuit by constructing the factory natively.
...
r.register(
    "org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext",
    "getWebServerFactory",
    "()Lorg/springframework/boot/web/server/servlet/ServletWebServerFactory;",
    |ctx, _args| {
        alloc_tomcat_factory(
            ctx,
            "org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory",
        )
    },
);
```

This native override is registered against `ServletWebServerApplicationContext` **unconditionally** — the base/generic servlet web-server context class used by *every* servlet backend (Tomcat, Jetty, Undertow), not just Tomcat apps. It was added (commit `3de9e929a`, May 11) to route around a *different*, real bug: CGLIB/condition-evaluation failing to complete Tomcat auto-configuration bean registration in Tomcat-based apps (`demo` SB 4.0.6, `sportme` SB 2.0.3, per the commit message), so `getBeanNamesForType(ServletWebServerFactory.class)` came back empty and the app threw `MissingWebServerFactoryBeanException` even though Tomcat *was* the correct/available backend. The fix hard-codes "always allocate `TomcatServletWebServerFactory`" as a blanket workaround.

For any Jetty-only (or Tomcat-absent) module, this shim fires on the exact same generic `getWebServerFactory()` call and tries to `ctx.new_object("org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory")` — a class that is genuinely absent from that classpath. `new_object`'s class resolution fails with `VmError::ClassFile(ClassFileError::ClassNotFound)`, and unlike the opcode-boundary paths (`Getstatic`/`Invokestatic`/`New`/`Checkcast`/etc., see `vm/src/runtime/exceptions.rs::convert_class_not_found` / `raise_no_class_def_found`), this native-shim call site has no such conversion — the internal `VmError` is never turned into a catchable Java `NoClassDefFoundError`/`ClassNotFoundException`. It propagates straight out of the native dispatch to the top-level `main-vm run()` and aborts the whole process, exactly as `docs/internal/springboot/applicationcontextrunnertests-lazy-cglib-classnotfound-crash-FIXED.md` previously documented and fixed for a different native call site (`native_class_for_name`). This shim is a second, unaddressed instance of the same "class-not-found inside a native bridge crashes the process instead of throwing a catchable Java exception" failure mode.

There are really two independent problems here, either of which alone would need fixing:

1. **The shim itself is architecturally wrong for non-Tomcat backends.** It should inspect which `ServletWebServerFactory` implementation is actually registered/available (or at minimum branch on whether the app is Jetty/Undertow-configured) instead of unconditionally hardcoding Tomcat. As written it silently breaks *every* Jetty (and presumably Undertow) app/test that hits this generic context method and doesn't have `spring-boot-tomcat` on the classpath — the crash is in the "servlet web server, but not Tomcat" case, which per the module split in Spring Boot 4.x (`spring-boot-web-server`/`spring-boot-jetty` are separate Gradle modules from `spring-boot-tomcat`) is an entirely ordinary, supported configuration.
2. **Even scoped correctly to Tomcat-only contexts, a class-load miss inside a native bridge must not abort the process.** `alloc_tomcat_factory`'s `ctx.new_object(impl_class)` call needs the same class-not-found → catchable-exception conversion that opcode-level resolution already gets, so a future similar shim (or this one under some other unanticipated classpath shape) fails safely as a Java exception instead of a fatal process abort.

## Repro

```powershell
cd C:\craton\CratonVM-spring-boot-crashfail-20260714
@"
module`tclass
module/spring-boot-jetty`torg.springframework.boot.jetty.autoconfigure.AutoConfigureWebServerJettyServletTests
module/spring-boot-web-server`torg.springframework.boot.web.server.context.MissingWebServerFactoryBeanFailureAnalyzerTests
"@ | Set-Content -Encoding utf8 .\single-class.tsv

powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -ClassList .\single-class.tsv `
  -Exe C:\craton\CratonVM-spring-boot-crashfail-20260714\target\release\cratonvm-spring-boot-rerun-20260716.exe `
  -RunName tomcat-shim-crossmodule-repro -Parallel 1 -TimeoutSec 300
```

Expected: both classes CRASH with `class file error: class not found: org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory` in the `.err.log`.

## Related

- `docs/internal/springboot/applicationcontextrunnertests-lazy-cglib-classnotfound-crash-FIXED.md` — the same "class-not-found inside a native bridge escapes as an internal VM error and crashes the process" failure mode, previously root-caused and fixed for `native_class_for_name`'s CGLIB proxy-initialization path. This doc is a second, unaddressed instance of that same class of bug, at a different native call site.
- `vm/src/runtime/exceptions.rs` (`raise_no_class_def_found`, `convert_class_not_found`) — the existing, correct pattern for converting a class-resolution miss into a catchable `NoClassDefFoundError` at opcode boundaries; the native shim in `net_phase_e.rs` needs the equivalent treatment.
- `native-builtins/src/net_phase_e.rs:6727-6780` (`alloc_tomcat_factory`, both `getWebServerFactory` registrations) — the defective shim.
- commit `3de9e929a` ("Round 57: native `ServletWebServerApplicationContext.getWebServerFactory` shim") — introduced the blanket Tomcat-only assumption; written against Tomcat-only apps (`demo`, `sportme`), never verified against a Jetty-only classpath.
- `docs/internal/CRATONVM_BUGS/BUG-G-classforname-never-throws-cnfe.md` — a related but distinct class-resolution-miss-handling issue (`Class.forName` synthesizing stubs instead of throwing CNFE), already fixed; same general area of the class manager but a different bug.
