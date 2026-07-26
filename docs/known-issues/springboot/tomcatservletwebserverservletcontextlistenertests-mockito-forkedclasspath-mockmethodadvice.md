# `TomcatServletWebServerServletContextListenerTests` — Mockito `MockMethodAdvice` `NoClassDefFoundError` inside `@ForkedClassPath` — OPEN (root cause now precise; see 2026-07-25 update)

## Update 2026-07-25 — root cause for the `WeakConcurrentMap` lookup miss precisely identified; one contributing defect fixed

Picked this up while closing out
[`devtools-2class-host-load-confound.md`](../../internal/fixed-suite-bugs/springboot/devtools-2class-host-load-confound-FIXED.md)
(that doc's 2026-07-24 update assumed this was "the SAME bug" — it is **not**;
see below). Reproduced the Tomcat symptom directly (not just the Jetty one)
against `TomcatServletWebServerServletContextListenerTests` on the fixture at
`/data/data/spring-boot-tomcat-crossmodule-20260717` (Azure host).

### What this update rules out

Exhaustively verified via direct native-level tracing (temporary
`MOCKADVICE_TRACE`-gated `eprintln!`s added and removed during
investigation) that ALL of the following are correct and NOT the cause:

- `System.identityHashCode()` for the mock — stable across every call,
  confirmed via `vm/src/vm/vm_exec.rs`'s `identity_hash_code`.
- `WeakReference`/`ReferenceQueue` clearing — `CRATONVM_DBG_WATCHREF` showed
  `gc::reference::process_weak_refs` never even ran during the failing test;
  separately, `expungeStaleEntries()`'s own `ReferenceQueue.poll()`
  (`native_rq_poll` in `native-builtins/src/reference.rs`) is a simple,
  correct per-instance linked-list pop with no cross-queue contamination.
- `map_hash_key`'s `hashCode()` dispatch (`native-collections/src/lib.rs`) —
  traced every call for `WeakKey`/`LatentKey` keys: identical, stable spread
  value for the same referent every time.
- The native `ConcurrentHashMap.get()`/`.put()` bucket-chain walk
  (`chm_seg_get`, `native_chm_get`, `native_chm_put`) — traced the actual
  `ConcurrentHashMap` *instance* identity involved in both `put()` (at mock
  creation) and `get()` (at verify time): it is the **same instance** both
  times, and the bucket-chain walk **does** find and return the stored
  `MockMethodInterceptor` entry.
- `equals()` dispatch on `WeakKey`/`LatentKey` — see the fix below; this WAS
  a real, confirmed defect (matching an existing precedent in this codebase,
  `native_brave_weak_key_equals`), but fixing it alone did not change the
  test's outcome.
- Spring's own `MockResolver` SPI (`SpringMockResolver`, registered by
  `spring-test.jar`'s `mockito-extensions/org.mockito.plugins.MockResolver`)
  — traced every `instanceof org.springframework.aop.SpringProxy` check it
  triggers via `AopUtils.isAopProxy()`: always correctly `false` for the
  ByteBuddy mock, so `MockUtil.resolve()` is a no-op here as expected.

### Fix applied (real defect, confirmed correct, but not sufficient alone)

`ConcurrentHashMap`'s native bucket-chain comparison
(`map_keys_equal` in `native-collections/src/lib.rs`) calls
`node_key.equals(lookup_key)` via `ctx.invoke_virtual(a, "equals", ..., [b])`
— a virtual dispatch to the STORED key's real bytecode `equals()` override.
For Mockito's `org.mockito.internal.util.concurrent.WeakConcurrentMap$WeakKey`/
`$LatentKey` (an asymmetric-equals pair identical in design to
`brave.internal.collect.WeakConcurrentMap$WeakKey`, which already has a
native bridge here for exactly this reason — see that registration's own
comment about "a shared `Object.equals` call site... cached for a different
receiver shape"), this dispatch was not reliably reaching the real bytecode.
Added matching native bridges mirroring Mockito's own `equals()` semantics
(decompiled via `javap` against `mockito-core-5.23.0.jar` to confirm exact
behavior):

- `native_mockito_weak_key_equals`
- `native_mockito_latent_key_equals`

Both registered in `native-builtins/src/reference.rs` for
`org/mockito/internal/util/concurrent/WeakConcurrentMap$WeakKey` /
`$LatentKey`'s `equals(Ljava/lang/Object;)Z`. Confirmed via tracing: these
now correctly return `true` for matching referents and `false` for
mismatched ones — the `ConcurrentHashMap.get()` lookup for the failing mock
DOES succeed with this fix (verified via a live `this`-instance +
`this_referent`/`other_key` cross-reference in the trace). This is a genuine
fix and is kept, but by itself it did **not** make
`TomcatServletWebServerServletContextListenerTests` pass — see below.

### Actual root cause of the residual (still OPEN)

Decompiled Mockito 5.23.0's actual exception source via `javap` to nail down
exactly which check throws: the observed exception text ("Make sure you
place the parenthesis correctly!" / "See the examples of correct
verifications:") is `Reporter.notAMockPassedToVerify()`'s specific message,
called directly from `MockitoCore.verify()`'s **first** check —
`mockingDetails(mock).isMock()` (i.e. `MockUtil.isMock(mock)` →
`getMockHandlerOrNull(mock)` returning `null`) — not the later
`assertNotStubOnlyMock`/`MockUtil.getMockHandler()` path.

Added `org/mockito/internal/util/MockUtil`, `MockingDetails`, `Plugins`,
`DefaultMockitoPlugins` to the existing `define_class` trace filter
(`classloading/src/class_manager.rs`) to see exactly which `ClassLoaderId`
resolves each Mockito-family class. Result, across the 2-test-method run:

```
org/mockito/internal/configuration/plugins/Plugins            -> UserDefined(3), Application, UserDefined(7)
org/mockito/internal/configuration/plugins/DefaultMockitoPlugins -> UserDefined(3), Application, UserDefined(7)
org/mockito/internal/util/MockUtil                             -> UserDefined(3), Application, UserDefined(7)
org/mockito/internal/util/DefaultMockingDetails                -> Application  (ONLY)
org/mockito/MockingDetails                                     -> Application  (ONLY)
```

`MockitoCore`/`InlineByteBuddyMockMaker`/`WeakConcurrentMap`/etc. showed the
same three-loader pattern as `MockUtil` above (`UserDefined(3)`,
`Application`, `UserDefined(7)` — one copy per forked test method, plus one
shared copy).

Per Spring's own `ModifiedClassPathClassLoader.loadClass(String)` (real
source, `test-support/spring-boot-test-support/.../ModifiedClassPathClassLoader.java`):

```java
@Override
public Class<?> loadClass(String name) throws ClassNotFoundException {
    if (name.startsWith("org.junit.") || name.startsWith("org.hamcrest.")
            || name.startsWith("io.netty.internal.tcnative.")) {
        return Class.forName(name, false, this.junitLoader);
    }
    String packageName = ClassUtils.getPackageName(name);
    if (this.excludedPackages.contains(packageName)) {
        throw new ClassNotFoundException();
    }
    return super.loadClass(name);
}
```

For a bare `@ForkedClassPath` test with no `@ClassPathExclusions`,
`org.mockito.*` is not excluded, so this correctly falls through to
`super.loadClass(name)` — i.e. `ClassLoader.loadClass(String, boolean)`'s
**standard JVMS parent-first delegation**: check `findLoadedClass`, delegate
to parent, only `findClass()` (define locally) if the parent can't provide
it. Since the parent (the real application/system loader) has `mockito-core`
on its classpath, EVERY Mockito class should correctly delegate to parent and
resolve to **one shared copy** — exactly what `DefaultMockingDetails`/
`MockingDetails` do (`Application` loader, only once, matching the shared
parent). That MockUtil/MockitoCore/InlineByteBuddyMockMaker/WeakConcurrentMap
instead get **independently re-defined under each fork's own loader**
(`UserDefined(3)`/`UserDefined(7)`, in addition to `Application`) is the bug:
it means `mock()` (executed inside the `@Bean` factory method, itself running
under the fork's own loader) registers its `MockMethodInterceptor` in *that
fork's own, separately-defined* `InlineByteBuddyMockMaker`'s
`WeakConcurrentMap` — while `verify()`'s `isMock()` check, reached via
`DefaultMockingDetails` (which correctly resolved to the single shared
`Application`-loader copy), consults `MockUtil`'s `Application`-loader copy
instead — a **different `defaultMockMaker` instance whose `WeakConcurrentMap`
never had this mock `put()` into it**. Every individual mechanism (hash,
equals, the map instance itself) is therefore provably correct in isolation
— they're just two different, disjoint `ConcurrentHashMap` instances.

**The actual defect is in CratonVM's native backing for
`ClassLoader.loadClass(String, boolean)`**, `cl_real_load_class_base` in
`native-builtins/src/classloader_real.rs`. Specifically this block (around
line 1185 in the 2026-07-25 `dev` tip):

```rust
if crate::classloader::url_classloader_isolated_from_app(ctx, this)
    && !crate::classloader::is_bootstrap_class_name(&internal)
    && !bootstrap_appended
{
    if let Some(result) = crate::classloader::ucl_try_define_local_class(ctx, this, &internal) {
        return result;
    }
    // -> ClassNotFoundException if not found in this loader's own URLs
}
```

This check runs **before** the "0. Real parent-first delegation" step further
down, and — per its own comment — is deliberately designed for the
`@ClassPathExclusions` case (a loader that has removed some JARs from its
URL list, where trying the parent first would "resurrect the excluded
class"). But `url_classloader_isolated_from_app(ctx, this)` returns `true`
for **any** `ModifiedClassPathClassLoader`, including a bare
`@ForkedClassPath` instance with **no exclusions at all** — whose URL list is
the full, unmodified classpath. For such a loader, this block short-circuits
straight to self-defining from its own (complete) URL list — skipping parent
delegation entirely for every class the loader's own URLs can satisfy, which
is nearly everything, including `org.mockito.*` — even though the *correct*,
JVMS/HotSpot-faithful, and Spring-source-intended behavior for a
non-excluded, non-special-cased package is to delegate to parent first and
only self-define if the parent can't provide it.

Why `DefaultMockingDetails`/`MockingDetails` escaped this and got the
(correct) shared-parent copy while everything else didn't is not yet
understood — a plausible guess is that those two classes happened to be
resolved via a different code path earlier (e.g. before
`ModifiedClassPathClassLoader` existed, during JUnit's own outer test
discovery/method-signature inspection), rather than via this `loadClass`
path at all; this was not confirmed and is worth checking first.

### Next steps for whoever picks this up

1. **Reproduce**: fixture at `/data/data/spring-boot-tomcat-crossmodule-20260717`
   (or an equivalent full spring-boot-project checkout with a built
   `module/spring-boot-tomcat/build/cratonvm-test-cp.txt`), then:
   ```bash
   cd /data/data/spring-boot-tomcat-crossmodule-20260717/module/spring-boot-tomcat
   CP="$(cat build/cratonvm-test-cp.txt):/data/data/spring-boot-tomcat-crossmodule-20260717/sb-runner"
   <EXE> --java-home /home/victor/jdk25 --Xmx 2g --stack-dump-on-timeout 0 \
     -Dfile.encoding=UTF-8 -Djava.awt.headless=true -Djava.io.tmpdir=/data/tmp \
     --add-opens=java.base/java.net=ALL-UNNAMED -cp "$CP" SbRunner \
     org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests
   ```
   (`-Djava.io.tmpdir=/data/tmp` works around this host's separately-documented
   full-root-filesystem issue; irrelevant to the bug itself.)
2. **Fix candidate**: `url_classloader_isolated_from_app`'s check in
   `cl_real_load_class_base` needs to distinguish "this loader has actually
   excluded something and must not resurrect it via the parent" from "this
   loader is merely an isolated-copy loader with an unmodified URL list and
   should behave like a normal parent-delegating `URLClassLoader` for
   anything it hasn't specifically excluded." The `excludedPackages`
   set/mechanism (visible on the Java side) may need a CratonVM-visible
   counterpart so this native check can tell the two cases apart, rather than
   using "is this an isolated URL loader at all" as the sole gate.
3. This function has **many** carefully-tuned edge cases already, each
   protecting against a previously-fixed regression (see its own extensive
   comments — `ClassPathOverrides`/`WithPackageResources`/
   `AggregatedClassLoader` cases, etc.). Any fix here needs a broad
   regression pass across the Spring Boot suite, not just this one class —
   deliberately **not** attempted as a blind patch in this session given the
   blast radius and time constraints. Documenting precisely instead, per
   this repo's own established convention for exactly this situation.
4. Confirm whether `DefaultMockingDetails`/`MockingDetails` really do take a
   different code path to get their correct resolution (see the open
   question above) — if so, that mechanism might point at the right fix
   more directly than modifying `cl_real_load_class_base`'s gate.

---

**Status: OPEN — found 2026-07-23.** Root cause narrowed but not fixed;
documenting rather than attempting a blind patch to Instrumentation/
self-attach code, which is shared by every ByteBuddy/CGLIB/Mockito-based
test in the whole Spring Boot suite (high blast radius).

## Symptom

`module/spring-boot-tomcat`'s
`org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests`
(`extends AbstractServletWebServerServletContextListenerTests`) FAILs both
of its test methods, each with the same cause:

```
org.springframework.beans.factory.BeanCreationException: ... Factory method
'servletContextListener' threw exception with message: Could not initialize
plugin: interface org.mockito.plugins.MockMaker (alternate: null)
  Caused by: java.lang.IllegalStateException: Internal problem occurred,
  please report it. Mockito is unable to load the default implementation of
  class that is a part of Mockito distribution. Failed to load interface
  org.mockito.plugins.MockMaker
    Caused by: java.lang.reflect.InvocationTargetException:
    java.lang.NoClassDefFoundError: org.mockito.internal.creation.bytebuddy.MockMethodAdvice
      Caused by: java.lang.NoClassDefFoundError
```

Both `@Test @ForkedClassPath` methods
(`registeredServletContextListenerBeanIsCalled`,
`servletContextListenerBeanIsCalled`, in the shared test-fixture base class
`AbstractServletWebServerServletContextListenerTests`) call
`Mockito.mock(ServletContextListener.class)` inside a `@Bean` factory
method, which is where this fires.

## Not a livelock — the previously-hung version of this class is fixed

This class was previously one of the 9 classes hitting the JUnit5
`InterceptingExecutableInvoker` speculative-layout-probe livelock, fixed
2026-07-18 (`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`).
That fix is confirmed still in effect: as of 2026-07-23 dev, this class runs
to full completion in ~276s (3 containers, 2 tests, both executed) with a
real `SBRUNNER_RESULT` line — it no longer hangs. What's left is a distinct,
newly-surfaced failure that the old livelock previously masked (the test
never got far enough to reach `Mockito.mock()` before).

## Reproduction

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe <binary> -JdkHome $env:JAVA_HOME `
  -ClassList <tsv with module/spring-boot-tomcat org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests> `
  -RunName repro -Parallel 1 -TimeoutSec 300
```

Passes cleanly on real HotSpot (confirmed 2026-07-23, `-Vm hotspot`, 6.7s).

## What's ruled out (confirmed this session via direct standalone repros, no JUnit involved)

1. **Plain `Mockito.mock()` in a bare `main()`** — works fine. Self-attach
   succeeds (`ByteBuddyAgent.install()`), the mock is created and callable.
2. **`Mockito.mock()` invoked via a child `URLClassLoader`** (`parent =
   system loader`, mirroring `ModifiedClassPathClassLoader`'s shape) —
   also works fine. Parent-first delegation means the mock class actually
   gets defined by the *parent* (system) loader either way, so a bare child
   loader alone isn't the trigger.
3. **Loading `MockMethodAdvice` in isolation before any Mockito init** does
   throw the analogous `NoClassDefFoundError: MockMethodDispatcher` — but
   that's *expected* in isolation (nothing has called
   `Instrumentation.appendToBootstrapClassLoaderSearch` yet to make the
   bootstrap-scoped dispatcher jar visible). Not the same failure as the
   real one, which happens *after* Mockito's own init sequence including
   self-attach has already run once (confirmed: exactly one "Mockito is
   currently self-attaching..." line in the `.err.log`, no repeated
   attempts, no visible self-attach error).

So the failure is neither "Mockito is broken" nor "child classloaders are
broken" in general — it's specific to the actual scenario:
`Mockito.mock()` called from *inside*
`ModifiedClassPathExtension.interceptTestMethod`'s reentrant nested-Launcher
execution (`@ForkedClassPath`'s mechanism — see
`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`
for the full mechanism description: a brand-new `Launcher.discover()`+
`execute()` for the same test runs *while the outer Launcher's own
`execute()` is still on the call stack*).

## Leading hypothesis (not yet confirmed)

`InlineByteBuddyMockMaker`'s constructor, after self-attach succeeds, builds
a temp JAR containing `MockMethodDispatcher.class` and calls
`Instrumentation.appendToBootstrapClassLoaderSearch(jarFile)` — routed by
CratonVM to `native_append_to_classloader_search0`
(`vm/src/runtime/instrument.rs:567`), which calls
`ctx.register_bootstrap_classpath(&[path])`
(`classloading::is_bootstrap_appended_class` then gates
`org/mockito/internal/creation/bytebuddy/inject/MockMethodDispatcher`
visibility — see the comment at
`native-builtins/src/classloader.rs:1792-1796`). `MockMethodAdvice`
references `MockMethodDispatcher` and fails to link if that registration
didn't take effect (or targeted an empty/invalid jar) by the time
`MockMethodAdvice` itself gets loaded.

Only one self-attach line appears in the log and no warning fires from
`appendToClassLoaderSearch0`'s native, so whatever's going wrong is silent
at the Rust level — either the append call never actually happens in this
code path (a different overload/version is invoked, or ByteBuddy takes an
early-return branch under this exact JDK/Mockito version combo), or the
temp dispatcher jar ByteBuddy builds at runtime (`Files.createTempFile`)
ends up empty/unreadable specifically when constructed from within the
reentrant nested-Launcher call stack.

## Next steps for whoever picks this up

1. Add temporary `eprintln!`/`tracing::debug!` tracing to
   `native_append_to_classloader_search0` (and its `appendToBootstrapClassLoaderSearch0`
   sibling) logging the exact path string and `is_bootstrap` flag every time
   either fires, then rerun just this one class and check whether the call
   happens at all, and with what path.
2. If it does fire with a plausible path, check whether the jar file on
   disk at that path actually contains `MockMethodDispatcher.class` at the
   time CratonVM registers it (a race between ByteBuddy writing the temp
   jar and CratonVM reading/indexing it as a classpath entry would explain
   a silent empty-jar registration).
3. If it never fires, the bug is upstream of the native — something about
   the reentrant Launcher stack makes ByteBuddy's own agent-installation
   code take a different (silently no-op) branch. Byte-buddy version on
   this classpath: 1.18.8 (mockito-core 5.23.0).

## Affected classes

| Module | Class | Method |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests` | `registeredServletContextListenerBeanIsCalled`, `servletContextListenerBeanIsCalled` |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerServletContextListenerTests` | `registeredServletContextListenerBeanIsCalled`, `servletContextListenerBeanIsCalled` |

Confirmed 2026-07-24 (worktree `CratonVM-spring-boot-jetty-closure-20260723`):
the predicted "likely affects any other `@ForkedClassPath` + `Mockito.mock()`"
scope above is real — `module/spring-boot-jetty`'s sibling class (same base
test fixture, `AbstractServletWebServerServletContextListenerTests`, just a
Jetty `webServerConfiguration` instead of Tomcat's) hits the SAME
`@ForkedClassPath`/reentrant-Launcher trigger with a DIFFERENT symptom:

```
org.mockito.exceptions.misusing.NotAMockException:
Argument passed to verify() is of type ServletContextListener$MockitoMock$xxx and is not a mock!
```

Root-caused one level further than the Tomcat symptom above (though still
not fixed): `Mockito.verify(mock)` → `MockUtil.isMock`/`getMockHandlerOrNull`
→ `InlineDelegateByteBuddyMockMaker.getHandler(Object)` looks the mock up in
`this.mocks: WeakConcurrentMap` (identity-keyed). The mock IS genuinely
created (the exception's own message names the real generated
`$MockitoMock$` class), so ByteBuddy/self-attach succeeded this time (no
`NoClassDefFoundError` here, unlike Tomcat's symptom) — but the later
`verify()` call's `WeakConcurrentMap.get(mock)` comes back empty, as if the
mock was never registered, or was registered against a different `MockMaker`
instance than the one `verify()` consults.

Ruled out via a fast standalone repro (mirrors the Tomcat doc's own ruled-out
list, retested independently): a plain `Mockito.mock()` + `verify()` cycle
run through an isolated `URLClassLoader` built the exact same way
`ModifiedClassPathClassLoader` builds one (`ManagementFactory.getRuntimeMXBean().getClassPath()`
fallback URL extraction, since the real system classloader isn't a
`URLClassLoader` on JDK9+) — passes cleanly on both HotSpot and CratonVM,
`isMock()` true before and after crossing the classloader boundary. So
"isolated classloader" alone isn't the trigger for either symptom; both
require the actual reentrant `Launcher.discover()+execute()` call while the
outer Jupiter engine's own `execute()` is still on the stack, exactly as this
doc's mechanism section already describes. Not fixed this session either —
same high-blast-radius reasoning as below applies (shared Mockito/self-attach
machinery, used by nearly every ByteBuddy/CGLIB-based test in the suite);
documenting the additional data point rather than risking a blind patch.

Likely affects any other `@ForkedClassPath`/`ModifiedClassPathExtension`
test that calls `Mockito.mock()` for the first time in that process from
inside the reentrant nested-Launcher context — confirmed independently
across 2 modules now (Tomcat, Jetty), each with a different visible symptom,
consistent with one shared underlying defect surfacing differently depending
on exactly where in Mockito's init/registration sequence it bites.
