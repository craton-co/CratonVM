# `*ApplicationContextRunnerTests` process CRASH: `$$SpringCGLIB$$` lazy-proxy class-not-found escapes as an internal error

**Status: OPEN. Severity HIGH — this is a full process CRASH (`main-vm run()
returned Err`, `std::process::exit(1)`), not a test FAIL.** The whole
CratonVM process aborts before any test result can be recorded, taking out
the entire class (and, if run with `-Parallel`, only that one process — but
still zero tests reported instead of a normal pass/fail/skip breakdown).

## Symptom

Three classes in `core/spring-boot-test` all abort with the byte-for-byte
identical error (confirmed by reading full stdout+stderr for all three):

- `org.springframework.boot.test.context.runner.ApplicationContextRunnerTests`
  (`.suite\results\crashfail-20260714\shard3\logs\core_spring-boot-test.org.springframework.boot.test.context.runner.ApplicationContextRunnerTests.err.log`)
- `org.springframework.boot.test.context.runner.ReactiveWebApplicationContextRunnerTests`
  (`shard3\logs\core_spring-boot-test.org.springframework.boot.test.context.runner.ReactiveWebAppli-9c4bbcd2e42d.err.log`)
- `org.springframework.boot.test.context.runner.WebApplicationContextRunnerTests`
  (`shard3\logs\core_spring-boot-test.org.springframework.boot.test.context.runner.WebApplicationCo-83c708ba6d6e.err.log`)

Full `.err.log` tail (identical across all three, only timestamps differ):

```
[cratonvm] main-vm run() returned Err: Error in thread "main" class file error: class not found: org/springframework/boot/test/context/runner/AbstractApplicationContextRunnerTests$ExampleProperties$$SpringCGLIB$$0
[cratonvm] main-vm run() Err (debug): Error in thread "main" class file error: class not found: org/springframework/boot/test/context/runner/AbstractApplicationContextRunnerTests$ExampleProperties$$SpringCGLIB$$0
```

`.out.log` is empty for all three (crash happens before any JUnit output is
flushed). Each of the three concrete test classes extends the abstract base
`AbstractApplicationContextRunnerTests`, which is where the failing nested
class actually lives — the class name in the error is the **full, untruncated**
literal string CratonVM produces; it is not cut off. (The task brief that
kicked off this investigation quoted a truncated form ending in a bare `$`;
that truncation was introduced when the error was summarized elsewhere, not
by CratonVM itself.)

## Root cause

### What Java code is actually running

`AbstractApplicationContextRunnerTests.java` (`apps\spring-boot\core\spring-boot-test\src\test\java\org\springframework\boot\test\context\runner\AbstractApplicationContextRunnerTests.java`,
around line 348) declares:

```java
@Configuration(proxyBeanMethods = false)
@EnableConfigurationProperties(ExampleProperties.class)
static class LazyConfig {
    @Bean
    ExampleBeanWithLazyProperties exampleBeanWithLazyProperties() {
        return new ExampleBeanWithLazyProperties();
    }
}

static class ExampleBeanWithLazyProperties {
    @Autowired
    @Lazy
    ExampleProperties exampleProperties;
}

@ConfigurationProperties
public static class ExampleProperties {
}
```

`ExampleProperties` is a plain `@ConfigurationProperties` POJO — it is
**not** `@Configuration`, and `LazyConfig` itself uses
`proxyBeanMethods = false`, so real Spring never runs
`ConfigurationClassEnhancer.enhance()` for either class. The CGLIB proxy
here comes from a completely different mechanism: Spring's
`@Lazy` field-injection support (`ContextAnnotationAutowireCandidateResolver`
/ `AutowireCandidateResolver.getLazyResolutionProxyIfNecessary`) builds a
lazy-initializing proxy of the target type. Since `ExampleProperties` is a
concrete class with no interfaces, Spring builds a **CGLIB subclass proxy**
via its internal (`org.springframework.cglib`) fork, using that fork's
current naming convention: `<Type>$$SpringCGLIB$$<n>` — confirmed in this
same worktree by
`docs\internal\fixed-suite-bugs\cglib-config-proxy-name-springcglib-aot-hint.md`,
which documents that "real Spring CGLIB ... may later define runtime AOP,
scoped, or generics proxies named `<Config>$$SpringCGLIB$$<n>`" (as opposed
to CratonVM's own synthetic `@Configuration` enhancer, which deliberately
uses the older `$$EnhancerByCGLIB$$` internal name to avoid collisions —
see `native-builtins\src\cglib_enhancer.rs` and
`native-builtins\src\lang_class.rs`'s `SPRING_CONFIG_CGLIB_MARKER` /
`CRATONVM_CONFIG_CGLIB_MARKER` constants around line 690-694).

So the class CratonVM is being asked to find,
`AbstractApplicationContextRunnerTests$ExampleProperties$$SpringCGLIB$$0`,
is a **real, dynamically-generated CGLIB bytecode class** that Spring's own
cglib fork is in the middle of creating for the `@Lazy` proxy — it was never
compiled to a `.class` file and was never expected to be found by a
classpath scan. The idiom real CGLIB generators use (see the extensive
comment already in `classloading\src\class_manager.rs` around lines
2753-2778) is: probe with `ClassLoader.loadClass(generatedName)` first
(cheap re-use check), catch `ClassNotFoundException`, and only then generate
+ `defineClass` the real bytecode. A first-generation lookup **must** report
"not found" in a way Java code can catch, or the generator can never run.

### Where CratonVM diverges

1. `native-builtins\src\classloader.rs`'s `is_cglib_proxy_name` (line
   1680-1685) recognizes only the **legacy** `$$EnhancerByCGLIB$$` token or
   the `net/sf/cglib/proxy/` package prefix, deliberately (per its own
   comment) to stay "STRICT" and avoid misfiring on Quarkus/ASM classes. It
   does **not** recognize `$$SpringCGLIB$$`, so this name is not routed to
   the safe stand-in (`java/lang/Object`) that guards other CGLIB
   defineClass short-circuits.

2. `classloading\src\class_manager.rs::load_class` has a deliberate,
   well-commented recoverable path for exactly this situation — "a name
   containing `$$` is the universal marker ... for a runtime-synthesized
   implementation that is never shipped as a `.class` file" (lines
   2753-2778) — which returns `ClassFileError::ClassNotFound` (recoverable,
   translatable to a Java `ClassNotFoundException`/`NoClassDefFoundError`)
   instead of fabricating a bogus synthetic stub. **But this path is only
   reached inside the `Err(_) if is_jdk_class(name) =>` arm** (line 2711).
   `is_jdk_class` (lines 6841-6856) matches `java/`, `javax/`, `sun/`,
   `jdk/`, `com/sun/`, array descriptors, and a fixed enterprise-stub
   allowlist (`org/jboss/...`, `org/wildfly/...`, etc. via
   `is_enterprise_stub_prefix`). **`org/springframework/...` is not in that
   allowlist.** So for this Spring-application-namespace generated proxy
   name, execution falls straight through to the unconditional catch-all
   `Err(e) => Err(e)` at line 2793 — functionally the same
   `ClassFileError::ClassNotFound` value, but reached via a path that never
   gets the special "this is an expected first-probe miss for a
   runtime-generated class" treatment the comment describes.

3. That raw `VmError::ClassFile(ClassNotFound)` is not translated into a
   Java `Throwable` at whatever call site triggered it (unlike ordinary
   constant-pool resolution misses, which get converted to
   `NoClassDefFoundError` via `raise_no_class_def_found` in
   `vm\src\runtime\exceptions.rs` lines 1515-1526/1642-1645). It instead
   surfaces as `MethodCallFailed::InternalError(e)`, which propagates
   uncaught through every calling frame — interpreter, JUnit reflection,
   `SbRunner` — all the way to `vm-cli\src\main.rs`'s top-level `run()`
   error handler:

   ```rust
   Err(e) => {
       eprintln!("[cratonvm] main-vm run() returned Err: {e:#}");
       eprintln!("[cratonvm] main-vm run() Err (debug): {e:?}");
       let _ = std::io::stderr().flush();
       std::process::exit(1);
   }
   ```

   (`vm-cli\src\main.rs` line ~3677-3682), which is exactly the crash
   signature observed in all three `.err.log`s, and is a generic top-level
   catch for `MethodCallFailed::InternalError` (`InternalError(e) =>
   bail!("Error in thread \"main\" {e}")`, line ~2802-2804) rather than
   `MethodCallFailed::ExceptionThrown`, which is the path for a genuine Java
   exception that JUnit/the test runner could have caught and reported as a
   normal test failure instead of crashing the process.

### Summary

The `$$` catch-all in `class_manager.rs::load_class` was written to make
exactly this class of failure (a framework's speculative
`ClassLoader.loadClass(generatedName)` probe before it generates the real
bytecode) into a normal, catchable Java exception instead of an internal
VM error — but it is gated behind `is_jdk_class(name)`, which excludes
`org/springframework/...` (and, by the same reasoning, any other
application-namespace package whose classes get CGLIB/ByteBuddy proxies at
runtime). Combined with `is_cglib_proxy_name` not recognizing the current
`$$SpringCGLIB$$` naming convention either, there is no path in this
codebase that treats a first-generation `$$SpringCGLIB$$` proxy-name lookup
under an application package as recoverable — it always becomes an
unhandled internal error that crashes the whole VM process. Any Spring
Boot test that exercises `@Lazy` field/constructor injection of a
concrete (non-interface) bean type — which is common well beyond these
three `*ApplicationContextRunnerTests` classes — is a plausible trigger for
the same crash.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Category all -Jit on `
  -RefreshLists -Start 1 -Count 0 `
  -RunName crashfail-20260714-repro
```

or, to isolate just these three classes against an existing class list
(`-ListOnly` first to confirm indices, then target them individually with
`-Start <n> -Count 1` per `apps\spring-boot-suite-runner\run-spring-boot-suite.md`),
each reproduces the identical crash on its own in ~5-10s wall-clock
(the crash happens during context-refresh in the first `@Lazy`-touching
test method, well before any other test in the class runs).

## Related

- `docs\internal\fixed-suite-bugs\cglib-config-proxy-name-springcglib-aot-hint.md`
  — the prior, already-FIXED bug that documents the `$$SpringCGLIB$$` vs
  `$$EnhancerByCGLIB$$` naming split for CratonVM's own **synthetic**
  `@Configuration` enhancer. That fix only touched `Class.getName()`
  aliasing for classes CratonVM itself enhances; it does not cover real
  Spring cglib-fork-generated proxies for `@Lazy`/AOP, which is what this
  bug is about.
- `docs\internal\spring\springsuite-0619-getbeanclassname-bean-filter.md`
  (bug-B2, OPEN residual) — a related but distinct CGLIB defect: real
  Spring instantiates a `...$$SpringCGLIB$$0` method-injection subclass
  fine, but CratonVM's own `@Configuration`-subclass instantiation silently
  returns a **null** bean instance (`IllegalArgumentException: Target
  object must not be null`) rather than crashing the process. Different
  mechanism (CratonVM's synthetic enhancer producing a bad instance vs.
  this bug, where a genuine dynamically-generated proxy class is never
  found and the failure escapes as an uncaught internal error), same
  general problem area (CGLIB proxy naming/handling).
- `native-builtins\src\classloader.rs`:
  - `is_cglib_proxy_name` (~line 1680) — the strict-match guard that does
    not recognize `$$SpringCGLIB$$`.
  - `cglib_guard_value` (~line 1718) and its `Lookup.defineClass` call site
    (~line 4986-4990) — the short-circuit this class name should probably
    also hit, but doesn't.
- `classloading\src\class_manager.rs`:
  - `load_class` (~line 2624), specifically the `is_jdk_class` gate at
    ~line 2711 and the `$$`-catch-all at ~line 2774 that never fires for
    application-namespace names.
  - `is_jdk_class` (~line 6841) — the allowlist that excludes
    `org/springframework/...`.
- `vm\src\runtime\exceptions.rs`:
  - `raise_no_class_def_found` (~line 1515) — the translation this failure
    should have gone through but apparently didn't at whatever call site
    triggered the `@Lazy` proxy lookup.
- `vm-cli\src\main.rs` (~line 3677-3682, ~line 2800-2804) — the top-level
  `InternalError` → process-abort path that is the actual crash mechanism.
