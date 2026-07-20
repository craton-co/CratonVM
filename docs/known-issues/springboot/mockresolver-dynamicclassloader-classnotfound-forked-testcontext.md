# Mockito `MockResolver` plugin `ClassNotFoundException` inside Spring's `@CompileWithForkedClassLoader` test context

**Status: OPEN — found 2026-07-20, investigated further 2026-07-20 (2nd session).
No deterministic repro found; likely timing/GC-pressure-dependent rather than
a clean classloader-delegation defect. Do NOT assume this is a simple
"missing native override" bug — every faithful isolated reconstruction of
the mechanism has worked correctly on CratonVM. See "2026-07-20 session 2"
below before spending more time on the classloader-delegation hypothesis.**

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

## 2026-07-20 session 2 — every isolated repro attempt SUCCEEDED; likely timing/GC-dependent

Built a fresh binary (`cratonvm-mockresolver-forked-classloader.exe`,
worktree `CratonVM-mockresolver-forked-classloader-20260720`, branch
`fix/mockresolver-forked-classloader-20260720`) and worked through every
hypothesis from session 1's "next steps" list. **None reproduced the bug in
isolation:**

1. **Resource-read-in-isolation probe** (`ForkProbe.java`, package
   `org.springframework.core.test.tools` to access the package-private
   `CompileWithForkedClassLoaderClassLoader`): constructed the REAL forked
   loader wrapping the REAL app classloader (itself loaded via the identical
   `--jar` pathing-jar-with-`Class-Path`-manifest mechanism SbRunner uses —
   confirmed this matters, see point 4 below), called
   `getResourceAsStream`/`loadClass`/`getResources("mockito-extensions/...")`
   directly on: the app loader, the bare forked loader, and the forked loader
   set as thread context loader. **All three succeeded identically to
   HotSpot** — `SpringMockResolver` resolves and reads correctly every time.
2. **Full `TestCompiler.forSystem().compile(...)` reconstruction**
   (`ForkProbe2.java`): decompiled `TestCompiler.compile()`'s real bytecode
   to confirm it does exactly `Thread.currentThread().setContextClassLoader(
   dynamicClassLoader)` around the compile callback (the REAL mechanism that
   puts a genuine `DynamicClassLoader` in context, matching the failing
   stack trace's frame). Ran the identical resource/class probes INSIDE that
   callback (context classloader = real `DynamicClassLoader` wrapping the
   real forked loader). **Still succeeded identically to HotSpot.**
3. **Concurrency probe** (`ForkProbe3.java`): 50 rounds × 8 threads, a FRESH
   `CompileWithForkedClassLoaderClassLoader` per round, all 8 threads racing
   `loadClass(SpringMockResolver)` simultaneously. **0/400 failures** — no
   classloading race in this mechanism alone.
4. **"Mockito touched via app loader before the fork" probe**
   (`ForkProbe4.java`, prompted by `[[reference_mockito_inline_mock_maker_redefinition_shadow_native]]`
   — the hypothesis that Mockito's inline-mock-maker retransforms a class
   process-wide, and code that worked in isolation can fail once that
   retransform has happened): pre-loaded `org.mockito.Mockito` via the app
   loader, THEN forked and created a real mock inside the forked/Dynamic
   loader context. **Still `MOCK OK`, no failure.**
5. **Pathing-jar `Class-Path` manifest size, as a false lead**: an EARLIER
   probe in this same session (`ResourceProbe.java` run with `-cp
   pathing.jar` instead of `--jar pathing.jar`) appeared to show
   `getResourceAsStream` returning `null` and 0 `mockito-extensions` entries
   — but this was a **test-methodology bug, not a CratonVM bug**: `-cp
   somejar.jar` does NOT follow that jar's own `Class-Path:` manifest
   attribute (only `-jar`/`--jar` does), so the "pathing jar" was
   contributing zero real classpath entries. Re-ran correctly with `--jar`
   at 2, 45, 65, 86, and 88 real classpath entries (the actual module
   classpath, `cratonvm-test-cp.txt`) — **all sizes work correctly**, ruling
   out a Class-Path-manifest-length/continuation-line parsing bug too.
   Lesson for next time: always sanity-check a pathing-jar repro against a
   DIRECT `-cp`/`-c` run of the same real jars before trusting a `-cp
   pathing.jar` result — if they disagree, suspect the harness, not the VM.

**The full `ChildManagementContextInitializerAotTests` run itself is flaky
across MULTIPLE distinct failure modes**, not just the one in this doc's
title: (a) the `ClassNotFoundException` documented above (session 1, 3-4/4
runs), and (b) in session 2, `org.opentest4j.AssertJMultipleFailuresError:
expected: 2 but was: 1` (a completely different assertion about the number
of management contexts started) on 2/2 runs, with a 3rd run killed by the
harness after 2 minutes (this test can be slow). At the time of session 2's
runs, this box had **~16 concurrent `cargo`/`rustc`/`cratonvm` processes and
50% CPU load from other sessions** (`tasklist`/CPU-load check, per
`[[feedback_shared_host_multitenant_confound]]`) — a real confound for
anything timing-sensitive.

**Working theory (unconfirmed):** this test exercises a LOT of concurrent,
GC-heavy machinery at once — in-memory `javac` compilation
(`TestCompiler`), dual web-server context startup (main + child/management),
and ByteBuddy mock codegen — inside a single JUnit method. Every hypothesis
that isolates ONE piece of that (classloader delegation logic, pathing-jar
parsing, classloading races, retransform-shadowing) has checked out clean.
The remaining, NOT-yet-tested candidate is a **GC-timing-dependent
corruption** specific to concurrent pressure from the FULL combination (this
codebase has a well-documented history of exactly this shape of bug — moving
GC relocating/corrupting classloader-adjacent state under concurrent load;
see the `TestCompiler` annotation cluster's "moving-GC residual" fixes and
the general `reference_stale_ref_decode_hardening` /
`reference_g1_parallel_evac_selfforward_uaf` pattern family) — OR the
`ClassCastException`/`ClassNotFoundException`/assertion-failure trio are
symptoms of ordinary test flakiness on an overloaded shared host rather than
distinct CratonVM bugs at all. **Do not trust further repro attempts on this
box without first checking `tasklist`/CPU load is low.**

## Next steps for a follow-up session

1. **Re-run on a quiet host first**, before any more diagnosis — confirm the
   failure-mode distribution (ClassNotFoundException vs assertion failure vs
   pass) is stable, not host-load noise.
2. If `ClassNotFoundException` still reproduces on a quiet host: since every
   piece in isolation works, the next candidate is GC-timing, not
   classloader logic. Run the REAL failing test (not an isolated probe, this
   needs the full concurrent/GC pressure) with `--verbose:gc` and/or a
   temporary `eprintln!` in `cl_get_resource_as_stream`
   (`native-builtins/src/classloader.rs`) guarded on the resource path
   containing "SpringMockResolver", to capture the ACTUAL receiver
   classloader's identity/state at failure time — something an isolated
   probe cannot fake, since it never reaches that exact concurrent state.
3. If the assertion failure (`expected: 2 but was: 1` management contexts)
   reproduces independently of the `ClassNotFoundException`, it may be a
   SEPARATE, genuine timing bug in dual-context startup worth its own doc —
   don't conflate the two just because they're in the same test class.
4. The 5 probe classes from this session
   (`ForkProbe.java`/`ForkProbe2.java`/`ForkProbe3.java`/`ForkProbe4.java`,
   package `org.springframework.core.test.tools`) are NOT currently checked
   into the repo (they all passed, so there was nothing to preserve as a
   failing repro) — if a future session wants them as a starting point for
   building a GC-pressure variant, they're straightforward to reconstruct
   from this doc's description; each is ~30-60 lines.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-actuator-autoconfigure` | `org.springframework.boot.actuate.autoconfigure.web.server.ChildManagementContextInitializerAotTests` |

Likely affects any other `@CompileWithForkedClassLoader` AOT test that
creates a Mockito mock/spy after the fork — not yet swept for other
instances in the suite.
