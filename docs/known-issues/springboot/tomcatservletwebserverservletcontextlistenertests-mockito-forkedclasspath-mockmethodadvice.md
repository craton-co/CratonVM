# `TomcatServletWebServerServletContextListenerTests` — Mockito `MockMethodAdvice` `NoClassDefFoundError` inside `@ForkedClassPath` — OPEN

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
