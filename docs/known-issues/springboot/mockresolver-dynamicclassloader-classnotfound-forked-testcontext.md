# Mockito `MockResolver` plugin `ClassNotFoundException` inside Spring's `@CompileWithForkedClassLoader` test context

**Status: OPEN — found 2026-07-20, root cause narrowed but not fixed.**

## Symptom

`ChildManagementContextInitializerAotTests` (and presumably any AOT test
using `@CompileWithForkedClassLoader` that later creates a Mockito mock) now
fails with:

```
Caused by: java.lang.IllegalStateException: Could not initialize plugin: interface org.mockito.plugins.MockResolver
  at org.mockito.internal.configuration.plugins.PluginLoader$2.invoke(PluginLoader.java:117)
  ...
  at org.springframework.boot.web.server.servlet.MockServletWebServer.initialize(MockServletWebServer.java:88)
Caused by: java.lang.IllegalStateException: Failed to load interface org.mockito.plugins.MockResolver implementation declared in java.util.Enumeration$Impl@...
  at org.mockito.internal.configuration.plugins.PluginInitializer.loadImpls(PluginInitializer.java:91)
  at org.mockito.internal.configuration.plugins.PluginLoader.loadPlugins(PluginLoader.java:105)
  at org.mockito.internal.configuration.plugins.PluginRegistry.<init>(PluginRegistry.java:49)
  at org.mockito.internal.configuration.plugins.Plugins.<clinit>(Plugins.java:26)
  at org.mockito.internal.MockitoCore.<clinit>(MockitoCore.java:74)
  at org.mockito.Mockito.<clinit>(Mockito.java:1810)
Caused by: java.lang.ClassNotFoundException: org.springframework.test.context.bean.override.mockito.SpringMockResolver
  at java.lang.ClassLoader.findClass(ClassLoader.java:673)
  at org.springframework.core.test.tools.DynamicClassLoader.findClass(DynamicClassLoader.java:89)
  at org.mockito.internal.configuration.plugins.PluginInitializer.loadImpls(PluginInitializer.java:84)
```

Found this while re-verifying
[`repeatablecontainers-method-cache-classcastexception-FIXED.md`](../../internal/springboot/repeatablecontainers-method-cache-classcastexception-FIXED.md) — that
doc's original `ClassCastException` no longer reproduces, but this class
still fails, now for this unrelated reason, at a LATER point in the test
(the AOT config-parsing phase where the original bug lived now succeeds
fine; this happens afterwards, when the test actually starts the mock
servlet web server and Mockito initializes for the first time).

## Context — the forked classloader chain

`ChildManagementContextInitializerAotTests` is an AOT test using Spring's
`@CompileWithForkedClassLoader`, which recompiles/reloads the test's own
classes fresh in an isolated classloader chain:

```
DynamicClassLoader
  -> CompileWithForkedClassLoaderClassLoader   (real Spring class, spring-core-test.jar)
       -> testClassLoader.getParent()          (e.g. the platform loader)
```

`CompileWithForkedClassLoaderClassLoader` (decompiled from
`spring-core-test-7.0.7.jar`, no source available locally) holds a private
`testClassLoader` field — the ORIGINAL, real test classloader (the one with
the full module test classpath, including `spring-test-7.0.7.jar`). Its
`loadClass(String)` override special-cases `org.junit`/`org.testng` (always
delegate to `testClassLoader` via `Class.forName`), and for everything else
does `super.loadClass(name)` — the REAL `java.lang.ClassLoader.loadClass`
parent-first algorithm, whose fallback (`findClass`) is ALSO overridden here:
it first tries a `classResourceLookup` callback (set by `DynamicClassLoader`
at construction, wired to the AOT-generated/dynamically-compiled class
files), and if that returns null, falls back to
`testClassLoader.getResourceAsStream(internalName + ".class")` — a raw
resource read (NOT a `loadClass` delegation) that mints a fresh `Class`
under the forked loader for ANY class the real test classloader can see,
library classes included. This is how `SpringMockResolver` (a real class in
`spring-test-7.0.7.jar`, not part of the AOT-generated sources) is meant to
be found: not via classloader delegation visibility rules, but via a direct
byte-read-and-`defineClass` through the captured `testClassLoader` reference.

Mockito's plugin loader (`PluginInitializer.loadImpl`/`loadImpls`,
`mockito-core-5.23.0.jar`) resolves `mockito-extensions/org.mockito.plugins.MockResolver`
(Mockito's OWN plugin-discovery convention, distinct from
`META-INF/services/`) via `Thread.currentThread().getContextClassLoader()
.getResources(...)`, reads the declared implementation class name
(`org.springframework.test.context.bean.override.mockito.SpringMockResolver`,
declared inside `spring-test-7.0.7.jar`'s own
`mockito-extensions/org.mockito.plugins.MockResolver` file), then calls
`classLoader.loadClass(name)` on that same context classloader — which, in
this test, is the `DynamicClassLoader`.

## What's confirmed

- **HotSpot**: this succeeds every time — `SpringMockResolver` loads fine
  through the chain described above.
- **CratonVM**: fails every time (3/3 runs on current `dev`, `d999dc76f`),
  with the exact stack trace above.
- Verified via `javap` that `spring-test-7.0.7.jar` genuinely has NO
  `META-INF/services/org.mockito.plugins.MockResolver` (only the Mockito-
  specific `mockito-extensions/` file) — ruling out a simple "wrong
  ServiceLoader convention" explanation.
- Verified via `javap` that `DynamicClassLoader` does NOT override
  `loadClass` at all (only `findClass`/`findResource`/`findResources`), and
  that `CompileWithForkedClassLoaderClassLoader` DOES override the public
  1-arg `loadClass(String)` directly (not just the protected 2-arg form).
- **Instrumented `cl_load_class_base_delegation`** (the Rust native that
  stands in for `java.lang.ClassLoader.loadClass`'s base parent-first
  delegation, `native-builtins/src/classloader.rs`) with `eprintln!`s
  guarded on the target class name, rebuilt, and reran: **zero output** —
  meaning `cl_load_class`/`cl_find_class`/`cl_load_class_resolve` (all three
  entry points that funnel into `cl_load_class_base_delegation`) are NEVER
  invoked for this class name during the failing run. The entire chain
  (`DynamicClassLoader`'s inherited base `loadClass`, the forked loader's
  own `loadClass` override, its `super.loadClass(name)` call, and its
  `findClass` override) runs as genuine interpreted/JIT-compiled REAL
  bytecode with **no CratonVM native classloader override intercepting
  anywhere in the chain** — confirmed by the real JDK source line number in
  the trace (`ClassLoader.java:673`, the real base `findClass`'s
  `throw new ClassNotFoundException(name)`).

This rules out the most likely-looking hypothesis (that CratonVM's
`check_override`-driven native `ClassLoader.loadClass`/`findClass`
substitution — see `vm/src/vm/vm_exec.rs` around the
`class_name == "java/lang/ClassLoader" && matches!((method_name,
descriptor), ("loadClass", ...) | ...)` block, and `native-builtins/src/
classloader.rs`'s `cl_load_class_base_delegation`, `receiver_overrides_
find_class`, `receiver_overrides_load_class_single/_resolve`,
`invoke_single_load_class_override` machinery — mis-delegates for this
loader chain). That machinery is simply never reached here; something
EARLIER (a different, not-yet-identified dispatch path — possibly a
JIT-compiled-callsite fast path in `vm/src/jit/helpers.rs`'s
`jit_invoke_dispatch`, which has its OWN, SEPARATE inline-cache/native-
shortcut logic parallel to the interpreter's `vm_exec.rs` path, not
inspected this session) is either running real bytecode when it should
route to the native, or the real bytecode itself has a bug specific to how
CratonVM represents `testClassLoader.getResourceAsStream(...)` for THIS
particular real classloader instance.

## Not yet investigated / next steps for a follow-up session

1. Confirm whether `testClassLoader.getResourceAsStream(
   "org/springframework/test/context/bean/override/mockito/SpringMockResolver.class")`
   succeeds in isolation (a tiny standalone repro: get the actual test/app
   classloader, call `getResourceAsStream` directly for this exact resource
   path) — this narrows whether the bug is in resource *reading* or in
   *reaching* that call at all (i.e. is `CompileWithForkedClassLoaderClassLoader
   .findClass`'s bytecode even being entered, and does its `classResourceLookup`
   callback maybe incorrectly return non-null garbage instead of null,
   short-circuiting before the `testClassLoader` fallback runs?).
2. Instrument (temporarily) `vm/src/jit/helpers.rs`'s `jit_invoke_dispatch`
   fast paths for `ClassLoader`/ `findClass`/`loadClass` call sites, since
   the interpreter-side native (`cl_load_class_base_delegation`) was proven
   NOT to be reached — the JIT's own dispatch cache is the next most likely
   place a similar (but separate) override-shortcut could be missing or
   wrong.
3. Reproduce with a MUCH smaller standalone Java program that manually
   constructs the same 3-classloader chain (`DynamicClassLoader` /
   `CompileWithForkedClassLoaderClassLoader` are both real, loadable
   classes from `spring-core-test-7.0.7.jar` — no full Spring Boot checkout
   needed) and calls `Mockito.mock(SomeInterface.class)` directly, to get a
   much faster iterate-and-rebuild loop than launching the full AOT test
   class each time (~6s+ per run through JUnit Platform).
4. Check whether this is the SAME underlying gap as the historical
   `bug-09-mockito-inline-mockmaker-selfattach.md` (kafka suite) self-attach
   work, or a genuinely new, separate plugin-loading gap specific to the
   NEWER `MockResolver` plugin type (added to Mockito for Spring's
   `@MockitoBean` support) — `MockMaker` plugin loading is confirmed
   working elsewhere in the suite, so this is likely SpringMockResolver /
   forked-classloader-specific, not a generic Mockito plugin-loading
   regression.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-actuator-autoconfigure` | `org.springframework.boot.actuate.autoconfigure.web.server.ChildManagementContextInitializerAotTests` |

Likely affects any other `@CompileWithForkedClassLoader` AOT test that
creates a Mockito mock/spy after the fork — not yet swept for other
instances in the suite.
