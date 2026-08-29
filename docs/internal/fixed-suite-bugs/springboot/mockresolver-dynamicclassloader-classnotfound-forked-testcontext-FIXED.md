# Mockito `MockResolver` plugin `ClassNotFoundException` inside Spring's `@CompileWithForkedClassLoader` test context

**Status: FIXED 2026-07-21.** This document's original forked-loader
identity issue was already repaired by the `Class.getDeclaredClasses()`
loader-aware lookup on `dev`. Revalidation then exposed and repaired the
remaining instrumentation residual: an explicitly-null `Class.forName`
loader rejected Mockito's `MockMethodDispatcher` even after
`Instrumentation.appendToBootstrapClassLoaderSearch` had appended its JAR.
`Class.forName(name, false, null)` now permits only names recorded from an
actual runtime-appended bootstrap JAR, and the complete affected Spring Boot
class passes with the Byte Buddy agent in both JIT and interpreter-only
execution. The historical investigation below is retained for provenance;
its open/residual statements are superseded by this resolution.

**Status: OPEN — found 2026-07-20, investigated 2026-07-20 (session 2),
root-caused and PARTIALLY FIXED 2026-07-21 (session 3). Session 2's "likely
GC-timing-dependent, not reproducible" verdict is SUPERSEDED — session 3
found a 100% deterministic, non-timing-dependent repro (a different symptom
on the SAME test: `AssertJMultipleFailuresError: expected 2 but was 1`, not
the `ClassNotFoundException` from session 1), root-caused it precisely to
`Class.getDeclaredClasses()` resolving nested classes through a loader-blind
lookup (fixed, see "Fix landed" below), and — after that fix let previously
DEAD code run for the first time — uncovered a SEPARATE, second bug in
generated-class-name plumbing for nested AOT management-context generation
(`public class class`/`public void void(...)` — literally-malformed
generated Java, NOT yet fixed). The test still fails; do not close this doc.
See "2026-07-21 session 3 continued — fix landed, second bug found" below.
The paragraph immediately below (original session-3 "not yet pinpointed"
writeup) is preserved for its diagnostic value but is SUPERSEDED by the
"continued" section further down, which found the actual site —
see "2026-07-21 session 3" below for what's ruled out and the precise next
step for a follow-up session.**

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
[`repeatablecontainers-method-cache-classcastexception-FIXED.md`](od-cache-classcastexception-FIXED.md) — that
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
`../../../../apps/META-INF/services/`) via `Thread.currentThread().getContextClassLoader()
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
  `../../../../apps/META-INF/services/org.mockito.plugins.MockResolver` (only the Mockito-
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

## 2026-07-21 session 3 — 100% deterministic repro found; root mechanism narrowed via real-HotSpot A/B; NOT yet fixed

Picked this back up per session 2's "re-run on a quiet host first" next
step. Host load was moderate (~30-80%, fluctuating with other concurrent
sessions) — re-ran `ChildManagementContextInitializerAotTests` repeatedly on
a fresh worktree/binary (`CratonVM-mockresolver-forked-classloader-20260721`,
branch `fix/mockresolver-forked-classloader-20260721`, binary
`cratonvm-mockresolver-forked-classloader-20260721.exe`, branched from
current `dev` tip `5c5718ff0`, well past session 1/2's `d999dc76f`).

**The `ClassNotFoundException` from session 1 did not reproduce even once
this session** (8+ runs). Every run instead hit the `AssertJMultipleFailuresError:
expected 2 but was 1` mode from session 2 — **100% of the time, completely
deterministically**, including across a from-scratch rebuild with added
instrumentation. This is the opposite of session 2's conclusion ("every
piece in isolation works, GC-timing dependent") — the aggregate failure mode
is NOT flaky; only the mix session 2 saw under heavy host load was noisy.

**Root cause, precisely characterized (not yet pinned to an exact line):**

The test's only assertion (`numberOfOccurrences("WebServer started", 2)`)
fails because the management (child) `ApplicationContext` never gets
created at all — `ChildManagementContextInitializer.start()` is never
invoked, because `ManagementContextAutoConfiguration.DifferentManagementContextConfiguration`
(the nested `@Configuration` class whose `@Bean childManagementContextInitializer()`
registers it) is never activated, because its guarding condition —
`@ConditionalOnManagementPort(ManagementPortType.DIFFERENT)`, backed by
`OnManagementPortCondition.getMatchOutcome()` — evaluates to **NO MATCH even
though `ManagementPortType.get(environment)` correctly computes `DIFFERENT`**
(confirmed both values print as `DIFFERENT` via temporary
`System.err.println`/`toString()` diagnostics patched directly into
`ManagementPortType`/`OnManagementPortCondition`/`ManagementContextAutoConfiguration`,
compiled into a side directory and classpath-prepended ahead of the real
jar — see "Reproducing this session's diagnostics" below).

The `NO MATCH` happens because `actualType == requiredType` (a plain enum
`==` in `OnManagementPortCondition`) is **false** despite both printing
`DIFFERENT` — **`requiredType.getClass() != actualType.getClass()`: two
distinct `Class<ManagementPortType>` objects exist in the same JVM
process**, one defined by `jdk.internal.loader.ClassLoaders$AppClassLoader`
(`requiredType`, sourced from `metadata.getIntrospectedClass()` — a
REFLECTION-based `StandardAnnotationMetadata` reading the real, already-
loaded `ManagementContextAutoConfiguration$DifferentManagementContextConfiguration`
class object) and one defined by
`org.springframework.core.test.tools.CompileWithForkedClassLoaderClassLoader`
(`actualType`, from `OnManagementPortCondition`'s own `ManagementPortType.get()`
call — `OnManagementPortCondition` itself is loaded via that forked loader,
per `context.getClassLoader()`, which Spring's `ConditionEvaluator` always
uses to resolve `@Conditional`'s condition-implementation class name,
independent of which loader defined the *candidate* class the condition is
being evaluated for).

**This generalizes the original doc's finding**: it is not specific to
Mockito's plugin loader or `SpringMockResolver` (a *library* class) — the
exact same forked-vs-app dual-definition mechanism also splits the identity
of `ManagementPortType`, an ordinary **application** class from this
module's own main sources, with a completely different, non-Mockito-related
symptom (a silently-skipped bean registration, not an exception).

**Confirmed via direct A/B against real HotSpot** (JDK 25,
`C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`, running the
SAME instrumented classes + same classpath, invoked directly with `java`
instead of through `SbRunner`/CratonVM): the test **passes** on HotSpot
(`SBRUNNER_RESULT tests=1 failed=0`), and the SAME diagnostic prints show
`metadata.getIntrospectedClass()` for `DifferentManagementContextConfiguration`
is **also** loaded via `CompileWithForkedClassLoaderClassLoader` on HotSpot
— i.e. on HotSpot, `ManagementContextAutoConfiguration` and its nested
`@Configuration` classes are consistently resolved through the SAME forked
loader as `OnManagementPortCondition`/`ManagementPortType`
(`requiredType.getClass() == actualType.getClass()` → `sameClass=true`),
while on CratonVM they split across two different loaders. **This proves
the split is a genuine CratonVM defect, not inherent to how
`@CompileWithForkedClassLoader` works** — some code path that runs the
REFLECTIVELY RE-INVOKED test method's body (`context.register(ManagementContextAutoConfiguration.class, ...)`
is a `ldc <class>` literal inside `aotContributedInitializerStartsManagementContext`'s
own bytecode) is, on CratonVM only, resolving that `ldc` against the WRONG
(original/app-loader) class identity instead of the actually-executing
(forked-loader-reloaded) copy of the test class that real HotSpot uses.

**Ruled out as the entry point** (each confirmed with a live, rebuilt,
env-var-gated `eprintln!` trace that fired zero times for
`ManagementContextAutoConfiguration`/`ManagementPortType`/
`WebEndpointAutoConfiguration` across multiple repro runs, including with
`--nojit`):
- `execute_ldc`'s `ConstantPoolEntry::ClassReference` arm
  (`vm/src/runtime/interpreter.rs`, both the `Ldc` and `LdcW` bytecode
  forms funnel through this one function) — traced unconditionally, zero
  hits. Rules out plain interpreted `ldc`/`ldc_w` as the resolution site.
- `resolve_class_loader_aware` (the loader-faithful `CONSTANT_Class`
  resolver used by `ldc`/`new`/`checkcast`/`instanceof`) — zero hits via its
  existing `CRATONVM_DBG_LOADER_TRACE` hook (extended with these class
  names for this session).
- `execute_invokestatic` (the slow/uncached `invokestatic` path, which has
  its own loader-aware `self_class_id`/`static_dispatch_class_id` logic —
  see the `Self-call identity fix` and `Sibling static owners` comments
  around `vm/src/runtime/interpreter.rs:28906-28927`) — zero hits via its
  existing `CRATONVM_INVOKESTATIC_LOADER_TRACE` hook (extended similarly).
  `ManagementPortType.get()` never reaches this path either — it's served
  from `execute_invokestatic_cached`'s thread-local `invoke_cache` on every
  observed call, including what should be the first (cold) call for a
  freshly-forked `OnManagementPortCondition` class — worth revisiting if a
  future session suspects `thread.invoke_cache`'s `(caller_class_id,
  cp_index)` keying.
- `Class.forName`/`ClassUtils.forName` (native, `lang_class.rs`'s
  `native_class_for_name`) — traced via the EXISTING `CRATONVM_FORNAME_TRACE`
  hook. Confirms `ChildManagementContextInitializerAotTests` itself gets
  resolved via `Class.forName` **twice**, with two DIFFERENT loader object
  pointers (consistent with the "test method is reflectively re-invoked via
  a fresh forked-loader copy of the test class" mechanism JUnit5's
  `CompileWithForkedClassLoaderExtension.interceptTestMethod` —
  an `InvocationInterceptor` — must be using), and that `OnManagementPortCondition`
  is loaded via the SECOND (forked) loader pointer — but
  `ManagementContextAutoConfiguration`/its nested classes are NEVER passed
  through `Class.forName` at all, ruling this out as their resolution path.
- JIT: re-ran the SAME repro with `--nojit` — bug persists identically
  (`sameClass=false`, same loaders, same failure). Rules out a JIT-compiled-
  method cache as the cause.

**Still unidentified**: whatever mechanism DOES resolve
`ManagementContextAutoConfiguration.class` (and its nested classes) inside
`aotContributedInitializerStartsManagementContext`'s bytecode — it is
provably reached (the class IS loaded, `getDeclaredClasses()` reflects it
correctly), it is provably NOT any of the five paths above. Remaining
candidates for a follow-up session, roughly in order of suspicion:
1. ~~`native_method_invoke`'s (`native-builtins/src/lang_class.rs:6371+`)
   `use_virtual_dispatch` branch~~ — **traced through by hand this session
   and looks sound, but not yet DISPROVEN by a live trace (only by static
   code reading)**: added a diagnostic print of `this.getClass().getClassLoader()`
   as the very first statement of `aotContributedInitializerStartsManagementContext`
   itself (compiled into a side directory, run standalone) — confirms the
   RECEIVER executing the test method is genuinely the forked-loader
   instance (`this.getClass().getClassLoader()` prints
   `CompileWithForkedClassLoaderClassLoader@...`, matching
   `TCCL@methodStart`), ruling out "Method.invoke was called against a
   stale app-loader receiver" as the explanation. `ctx.invoke_virtual`
   (`vm/src/vm/vm_exec.rs:7735`) → the non-lambda branch's
   `needs_exact_class_dispatch` gate (~line 8398, `resolved_from_receiver
   && (... || get_loaded_class_id(&class_name) != Some(receiver_class_id))`)
   → `invoke_on_class_shared` (~line 13613) → `find_method_recursive` →
   `interpreter::execute(..., declaring_class_id, ...)` (~line 17911) all
   read as correctly threading the PRECISE `receiver_class_id` through by
   hand-tracing the code, with no obvious name-collapse point — but this
   was NOT verified with a live trace of `declaring_class_id`/the pushed
   frame's actual `class_id` at the point `aotContributedInitializerStartsManagementContext`
   itself starts executing. **That is the single most valuable next
   diagnostic**: an `eprintln!` right where `interpreter::execute` pushes
   the new frame (or at the top of `execute()` itself), printing
   `declaring_class_id` and its loader, guarded on
   `method_name == "aotContributedInitializerStartsManagementContext"` —
   if that ClassId is already wrong (app-loader) at frame-push time, the
   bug is upstream of `execute_ldc` as expected and somewhere in this
   dispatch chain despite it reading clean; if it's correctly forked at
   frame-push time, the bug is NOT in dispatch at all and must be in
   constant-pool/instruction representation itself (candidate 2).

   **Done this session** (no rebuild wasted — this was the single most
   informative trace added): put that exact `eprintln!` at the very top of
   `interpreter::execute` (`vm/src/runtime/interpreter.rs:4442`, guarded on
   `method_name == "aotContributedInitializerStartsManagementContext"`,
   env var `CRATONVM_EXEC_FRAME_TRACE`). **Zero hits — including with
   `--nojit`.** This is a bigger finding than it first looks: it means
   `aotContributedInitializerStartsManagementContext`'s bytecode is NEVER
   executed via the canonical `interpreter::execute` entry point at all,
   for either the app-loader or forked-loader copy, for the whole test run.
   Combined with `execute_ldc` also never firing, the reflective
   `Method.invoke` call for THIS test method must be dispatching through a
   path that never reaches `interpreter::execute`/`execute_ldc` — i.e.
   `native_method_invoke`'s `use_virtual_dispatch` branch's
   `needs_exact_class_dispatch` gate (`vm/src/vm/vm_exec.rs:8398`) is
   probably evaluating **false** here (contrary to the by-hand trace
   through the code above, which assumed it would be true), sending
   dispatch through `self.invoke_or_native(&class_name, ...)` instead of
   `invoke_on_class_shared` — and `invoke_or_native`
   (`vm/src/vm/vm_exec.rs`, a large separate function starting somewhere
   around line 10500, not read in detail this session) apparently has ITS
   OWN separate bytecode-execution call site that doesn't funnel through
   `interpreter::execute`. **This is now the concrete next step**: trace
   (or read) `invoke_or_native`'s own dispatch/execute call, and/or add an
   `eprintln!` right at `native_method_invoke`'s `needs_exact_class_dispatch`
   check (`vm/src/vm/vm_exec.rs:8398`) to see which branch it actually
   takes and what `get_loaded_class_id(&class_name)` vs `receiver_class_id`
   evaluate to for this exact call — that will show directly whether the
   gate itself is the bug (evaluating false when it should be true) or
   whether `invoke_or_native` has its own separate loader-identity gap.

   **Done this session too — deepens the mystery further.** Added that
   exact trace at `vm_exec.rs:8398` (env var `CRATONVM_NEEDS_EXACT_TRACE`)
   and rebuilt. **Also zero hits** — even though the EXISTING
   `CRATONVM_IAE_TRACE` hook a little earlier in `native_method_invoke`
   (`native-builtins/src/lang_class.rs:6744`) confirms
   `native_method_invoke` DOES run for this exact call and computes
   `is_static=false use_virtual=true` (so it does take the
   `ctx.invoke_virtual(recv, ...)` branch at ~line 6770). So: `invoke_virtual`
   is entered, but returns from somewhere BEFORE reaching line 8398 —
   through the lambda-proxy branch (shouldn't apply — the receiver is an
   ordinary object), the `java.lang.reflect.Proxy$Instance`/annotation-proxy
   special cases (also shouldn't apply), or some other early-return between
   the "not lambda" branch's start (~line 8188) and line 8398 not read
   closely this session. **Next diagnostic** for whoever picks this up:
   put a bare unconditional `eprintln!` (or a counter) at the very top of
   the "not lambda" branch (right after the `else {` around line 8188,
   before `resolved_from_receiver`/`class_name` get computed), guarded on
   the same method-name check, to confirm execution even gets that far —
   then walk forward from there line by line (there's a lot of code between
   8188 and 8398 this session did not read in detail) rather than jumping
   straight to the gate as this session did.
2. A parsed-classfile/constant-pool structure interning or dedup mechanism
   keyed by content (bytes) rather than by `ClassId` — if CratonVM ever
   reuses the SAME `Arc`/`Rc`-shared `ClassFileMethod`/constant-pool data
   for two `defineClass` calls that happen to submit byte-identical class
   files (exactly what happens here: the forked loader's `findClass`
   fallback reads and defines the SAME `.class` bytes the app loader
   already defined), any resolved-reference cache living on that shared
   structure would leak across the two otherwise-distinct `ClassId`s. Not
   directly located this session (searched `classloading/src/class_manager.rs`
   and `class.rs` for evidence of this and found none obviously — the
   `ConstantPoolEntry::ClassReference` variant itself carries only a raw
   name, no resolved-cache field — but a decoded-`Instruction` cache
   elsewhere in the interpreter, populated once per method and reused
   across invocations, was NOT ruled out and is the most likely remaining
   candidate given `execute_ldc` is proven unreached).

### Reproducing this session's diagnostics

The instrumentation lives only in the (uncommitted, disposable) worktree
`CratonVM-mockresolver-forked-classloader-20260721/diag/` this session —
not checked in, since it's throwaway `System.err.println` patches to real
JDK/Spring classes, not a CratonVM fix. To reconstruct: copy
`ChildManagementContextInitializer.java`, `ManagementPortType.java`,
`ManagementContextAutoConfiguration.java`, `OnManagementPortCondition.java`
(all from `apps/spring-boot/module/spring-boot-actuator-autoconfigure/src/main/java/org/springframework/boot/actuate/autoconfigure/web/server/`)
and `MockServletWebServer.java`, `MockServletWebServerFactory.java` (from
`apps/spring-boot/module/spring-boot-web-server/src/testFixtures/java/org/springframework/boot/web/server/servlet/`)
into a side directory, add diagnostic prints (class-loader identity,
`requiredType`/`actualType` reference-equality + `getClass()`/loader dumps
in `OnManagementPortCondition.getMatchOutcome`), `javac` them against the
module's `cratonvm-test-cp.txt`, and run `SbRunner` with that side directory
PREPENDED on the classpath (ahead of the real jar) so the patched classes
shadow the real ones — both via `cratonvm.exe` and via plain `java.exe`
(real JDK) for the A/B comparison. The three Rust-side traces added this
session (`CRATONVM_LDC_CLASSREF_TRACE` in `execute_ldc`, an extended
`CRATONVM_DBG_LOADER_TRACE` guard in `resolve_class_loader_aware`, and an
extended `CRATONVM_INVOKESTATIC_LOADER_TRACE` guard in `execute_invokestatic`
— all additive, env-var-gated, zero behavior change when unset) ARE
committed on this branch and available for reuse.

## 2026-07-21 session 3 continued — root cause found and fixed; second bug uncovered

**Process note first, because it cost most of this continued session and is
worth not repeating**: every `cargo build` after the very first one in this
session had been silently no-op'ing. The rebuild command was `cmd /c
'"...\vcvars64.bat" && ... && cd /d <worktree> && cargo build ...'` issued
through the **Bash tool** — this is exactly
[[reference_bash_tool_cmd_c_msys_trap]]: Git-Bash/MSYS mangles `cmd`'s `/c`
flag (worse, with a `cd /d <drive-letter-path>` segment in the string, per
that memory's "non-deterministic trigger" note) and the whole invocation
silently degrades to an interactive `cmd.exe` banner + bare prompt — no
error, exit code still reported as 0 by the wrapping tool call. `cratonvm.exe`
kept the FIRST build's mtime through six subsequent "successful" rebuilds;
every `eprintln!` trace added in this window (`CRATONVM_LDC_CLASSREF_TRACE`,
the `resolve_class_loader_aware`/`execute_invokestatic` guard extensions,
`CRATONVM_EXEC_FRAME_TRACE`, `CRATONVM_NEEDS_EXACT_TRACE`,
`CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE`) reported "zero hits" not because
those code paths were unreached, but because **the binary being tested
never contained them**. All of the "ruled out" conclusions attributed to
those traces earlier in this doc are therefore unverified, not
disproven — the paths may well be fine (the eventual real trace, below,
suggests they are), but treat that whole stretch as informative context,
not fact. **Lesson applied**: switched to the **PowerShell tool** for every
`vcvars64.bat`/`cmd /c` build chain from this point on (per the memory's
"working fix"), verified with `stat -c "%y %n" target/release/cratonvm.exe`
that the mtime actually advanced and the log ended with `Finished \`release\`
profile` before trusting any subsequent result. Every trace result below is
from a build verified this way.

With a genuinely rebuilt binary, the SAME traces immediately told a clean,
consistent story: `invoke_virtual` IS entered (not lambda), the loader-
identity gate at `vm_exec.rs:8398` DOES correctly select
`invoke_on_class_shared` with the receiver's own (forked) `ClassId`,
`interpreter::execute` DOES push the frame for
`aotContributedInitializerStartsManagementContext` with the correct forked
`class_id` and `loader=Some(UserDefined(3))`, and `execute_ldc` DOES resolve
its `ldc ManagementContextAutoConfiguration.class` through
`resolve_class_loader_aware` with the correct forked `referencing_class_id`
— which correctly drives the forked loader's own `loadClass` and gets back
a **forked** `ManagementContextAutoConfiguration`. Every step in the
dispatch chain for the OUTER class was already correct — candidate 1 from
the earlier (stale-binary) writeup is a dead end, not a real lead.

The actual bug is one level down: **`Class.getDeclaredClasses()`**
(`native_class_get_declared_classes`,
`native-builtins/src/lang_class.rs:15537`) resolves each NESTED class
(`SameManagementContextConfiguration`, `DifferentManagementContextConfiguration`,
`LocalManagementPortPropertySource`) via a plain, loader-blind
`ctx.class_id_by_name(inner_class)` — a straight call to
`ClassManager::find_class_by_name`, which only walks the builtin
bootstrap→extension→application delegation chain (see
`get_loaded_class_id`/`get_loaded_class_id_for_requester` in
`classloading/src/class_manager.rs:1871+`) and never even looks at
user-defined loaders. So even though `ManagementContextAutoConfiguration`
itself (the OUTER class, resolved via the correct, loader-aware `ldc` path
above) is the FORKED copy, asking IT for its declared/nested classes
returned the APP LOADER's copies of those nested classes — confirmed
directly: `metadata.getIntrospectedClass()` for
`DifferentManagementContextConfiguration` printed
`jdk.internal.loader.ClassLoaders$AppClassLoader`, while
`OnManagementPortCondition`/`ManagementPortType.get()` (reached via a
completely different path — `Class.forName(name, context.getClassLoader())`,
already loader-aware) printed the forked loader — two provably-different
`Class<ManagementPortType>` objects, so the `==` check in
`OnManagementPortCondition.getMatchOutcome()` silently failed.

There's an EXISTING, precedented fix for exactly this shape:
`NativeContext::class_id_by_name_near(name, near)`
(`native-api/src/registry.rs:1496`) — its own doc comment describes this
exact bug class almost verbatim (a Hibernate ByteBuddy-reloaded
`@EmbeddedId` class collapsing to the wrong loader's copy). It wasn't wired
up in `getDeclaredClasses()`. **Fix, in two parts** (both needed —
verified the first alone was insufficient):

1. Swap `ctx.class_id_by_name(inner_class)` for
   `ctx.class_id_by_name_near(inner_class, class_id)` (`class_id` = the
   OUTER class being reflected on) for the "already loaded" fast path. This
   alone did NOT fix the test: `class_id_by_name_near` only prefers an
   ALREADY-loaded same-loader copy; if the forked loader was never actually
   asked to load `DifferentManagementContextConfiguration` (nothing else in
   the run happens to trigger that — the outer class gets its own forked
   identity via a directly-executed `ldc`, but Java-level reflection on it
   via `getDeclaredClasses()` never itself calls `loadClass` on anything),
   there IS no forked-loader copy yet to prefer, and the lookup still fell
   through to the global (app-loader) one.
2. So, for a lookup miss, DRIVE the outer class's own defining loader's
   `loadClass(String)` directly (JVMS §5.4.3 initiating-loader semantics) —
   the same pattern `drive_defining_loader_load` uses in
   `vm/src/runtime/interpreter.rs`, reimplemented locally in
   `native-builtins` (that function is private to the `vm` crate; `native-
   builtins` sits below it in the dependency graph and can't call it
   directly, but has the same building blocks —
   `crate::classloader::defining_loader_for` for the loader object,
   `ctx.invoke_virtual(loader_obj, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", ...)`
   to actually drive it) — BEFORE falling back to the global, loader-blind
   `ctx.load_class(inner_class)`. This is what actually made the forked
   loader mint its own `DifferentManagementContextConfiguration` (via its
   `findClass` fallback — the raw byte-read-and-`defineClass` mechanism
   from the very top of this doc, this time correctly triggered).

**Verified fixed**: re-ran the diagnostic build with both parts of the fix.
`OnManagementPortCondition`'s `sameClass` print flipped from `false` to
`true` for BOTH `DifferentManagementContextConfiguration` and
`SameManagementContextConfiguration`, `requiredType`/`actualType` now share
one `Class` object, and `@ConditionalOnManagementPort(DIFFERENT)` correctly
MATCHES. **This part of the bug is genuinely fixed** — landed in
`native-builtins/src/lang_class.rs` on this branch.

**The test still fails, on a different, previously-unreachable error.**
With the condition now correctly matching, `DifferentManagementContextConfiguration`'s
bean gets registered for the first time ever under CratonVM for this test,
which drives `ChildManagementContextInitializer.processAheadOfTime()` (the
`BeanRegistrationAotProcessor` override) to run its own NESTED AOT
generation pass for the management child context
(`new ApplicationContextAotGenerator().processAheadOfTime(this.managementContext, managementGenerationContext)`
inside `AotContribution.applyTo`, see `ChildManagementContextInitializer.java`
near the end of this doc's earlier reading). That nested pass's generated
source is malformed:

```
@@Generated
public class class implements ApplicationContextInitializer<GenericApplicationContext> {
  @@Override
  public void void((GenericApplicationContext  applicationContext) {
```

Note the literal Java keywords used as identifiers (`class class`, `void
void`), doubled `@@Generated`/`@@Override` annotations, and a doubled `((`
in the method signature — this is javapoet-generated code where the
GENERATED class name and method name came back empty/null, so the template
literally inserted the modifier keywords with nothing after them. This
throws `com.thoughtworks.qdox.parser.ParseException: syntax error @[15,14]`
inside `SourceFile.getClassName()` when Spring's `TestCompiler` tries to
QDox-parse the generated content to derive its class name — i.e. it never
even reaches `javac`. This is a SECOND, INDEPENDENT CratonVM bug (almost
certainly in how a nested/secondary `ApplicationContextAotGenerator.processAheadOfTime`
pass derives or returns its generated `ClassName`, or in whatever CratonVM
reflection/String machinery javapoet's `TypeSpec`/`MethodSpec` builders rely
on for name formatting) — NOT a consequence of the `getDeclaredClasses()`
fix being wrong, but a pre-existing gap that the `getDeclaredClasses()` bug
had been accidentally masking by keeping this whole code path dead (the
condition always evaluated NO MATCH before, so `DifferentManagementContextConfiguration`,
and everything downstream of it including this nested AOT pass, never ran
at all under CratonVM until now).

**This doc stays OPEN.** The `getDeclaredClasses()` fix is real, safe, and
worth keeping (fixes a genuine, general loader-identity bug, not just this
test), but the test's own assertion still fails on the newly-exposed second
bug. A follow-up session should start from the malformed-generated-source
symptom above — find where `ApplicationContextAotGenerator.processAheadOfTime`'s
returned `ClassName` (or whatever javapoet consumes to name the generated
class/method) comes back empty specifically for this NESTED/secondary AOT
pass (the OUTER `processAheadOfTime` call at the top of the test works
fine — its own generated sources dumped cleanly in this doc's earlier
session-3 "generated sources" excerpt — so whatever's different about the
nested pass, triggered from inside a `BeanRegistrationAotProcessor`
callback rather than directly from the test, is the next thing to isolate).

## Next steps for a follow-up session

**Superseded**: the numbered candidates below (1-3, from the stale-binary
stretch of session 3) turned out to be dead ends once traced with a
genuinely rebuilt binary — `invoke_virtual`/`execute_ldc`/frame setup are
all correct for the outer class. The real bug (`getDeclaredClasses()`) is
described and FIXED in "session 3 continued" above. **Start here instead**:

0. Isolate the malformed-generated-source bug ("session 3 continued"'s
   `public class class` / `public void void(...)` symptom) — this is now
   the ONLY thing standing between this test and passing. Suggested
   approach: write a minimal standalone repro that calls
   `new ApplicationContextAotGenerator().processAheadOfTime(...)` on a
   plain `GenericApplicationContext` from INSIDE a
   `BeanRegistrationAotProcessor.processAheadOfTime` callback (mirroring
   `ChildManagementContextInitializer`'s exact shape) rather than directly
   from a test method, and dump the `ClassName` it returns — compare against
   the OUTER, directly-invoked call's `ClassName`, which works fine. Prime
   suspects: something about `RegisteredBean`/`BeanRegistrationCode`
   context available to the OUTER call but not reconstructed correctly for
   a nested call issued from inside a processor callback, or a CratonVM
   naming/reflection utility javapoet depends on (`Class.getSimpleName()`,
   `String` formatting, or similar) behaving differently when called from
   that nested stack depth/context.
1. (Superseded — kept for record) Start from candidate 1 above (`native_method_invoke`'s virtual-dispatch
   branch / `ctx.invoke_virtual` / frame setup for a reflectively re-invoked
   instance method) — add a trace at the point a new frame is pushed for a
   virtually-dispatched call, confirm whether the pushed frame's `class_id`
   matches the receiver's actual (forked) `ClassId` or a stale one.
2. (Superseded — kept for record) If that's clean, hunt for a decoded-`Instruction`/bytecode cache that
   might be shared by content-hash across the two `defineClass` calls
   (candidate 2) — this is the most likely remaining explanation given
   `execute_ldc` (the function that would consume such a cache miss) is
   proven never reached for these names.
3. (Superseded — kept for record) Once the exact resolution site is found, the fix is almost certainly the
   same *shape* as the already-existing "Use the Method object's OWN
   already-resolved declaring ClassId" fix in `native_method_invoke`'s
   static-method branch, or the same shape as the already-fixed
   `reference_vtable_fast_dispatch_redefine_staleness` bug (a cache/lookup
   not scoped precisely enough to the exact `ClassId`, collapsing two
   structurally-identical-but-distinct classes onto one) — extend whichever
   mechanism is found to be loader/ClassId-precise rather than name- or
   content-keyed.
4. The `ClassNotFoundException` from session 1 remains formally
   unreproduced since session 2 (3 sessions, 0 repros since 2026-07-20
   session 1's original 3-4/4). It may already be fixed as a side effect of
   unrelated `dev` progress since `d999dc76f`, or may need very specific
   timing this session's runs didn't hit. Do not spend further time chasing
   it specifically — if it resurfaces, it's likely the SAME root mechanism
   (dual class identity under `@CompileWithForkedClassLoader`) manifesting
   as an exception instead of a silently-skipped bean, once `Mockito`'s
   static init happens to run inside a frame with the wrong identity instead
   of `OnManagementPortCondition`'s enum comparison.
5. The 5 probe classes from session 2
   (`ForkProbe.java`/`ForkProbe2.java`/`ForkProbe3.java`/`ForkProbe4.java`,
   package `org.springframework.core.test.tools`) are still available at
   `docs/known-issues/repros/mockresolver-forked-classloader/` if useful,
   though session 3's finding suggests they wouldn't reproduce this specific
   bug anyway (they don't exercise a reflectively re-invoked TEST METHOD
   itself, only direct classloading/compile calls).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-actuator-autoconfigure` | `org.springframework.boot.actuate.autoconfigure.web.server.ChildManagementContextInitializerAotTests` |

Likely affects any other `@CompileWithForkedClassLoader` AOT test that
creates a Mockito mock/spy after the fork — not yet swept for other
instances in the suite.
