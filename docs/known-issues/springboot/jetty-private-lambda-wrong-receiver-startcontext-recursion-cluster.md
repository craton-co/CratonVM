# Jetty embedded-server startup: private `this::lambda$doStart$0` method references dispatch onto a same-named private method in a *subclass* instead of the exact declaring class, causing `ServletContextHandler`/`WebAppContext` to re-run `startContext()` — manifests as `StackOverflowError`, duplicate servlet/filter registration, or a slow multi-minute-per-test hang

**Status: OPEN — found 2026-07-17**

## Symptom

11 of 12 `module/spring-boot-jetty` classes in this rerun batch fail or hang
starting the embedded Jetty server (the 12th, `LoaderHidingResourceTests`,
is an unrelated jar-filesystem-listing bug — see the sibling doc
`jetty-loaderhidingresourcetests-empty-jar-listing.md`). All 11 share one
root mechanism but present in three different outward shapes depending on
which handler variant is exercised and how many recursive iterations run
before something (a duplicate-registration guard, or the OS stack limit)
stops it:

| Class | Status | Shape |
|---|---|---|
| `AutoConfigureWebServerJettyReactiveTests` | FAIL | `StackOverflowError` |
| `JettyServerPropertiesTests` | FAIL | `StackOverflowError` (x3, once per test method) |
| `JettyMetricsAutoConfigurationTests` | FAIL | mixed: `StackOverflowError` + `NullPointerException` (`FilterRegistration$Dynamic.setAsyncSupported`) across different test methods |
| `JettyReactiveWebServerAutoConfigurationTests` | FAIL | `StackOverflowError` (x3) |
| `JettyReactiveWebServerFactoryTests` | FAIL | `StackOverflowError` |
| `AutoConfigureWebServerJettyServletTests` | FAIL | `IllegalStateException: Failed to register 'servlet servlet' ... Possibly already registered?` |
| `JettyServletWebServerAutoConfigurationTests` | FAIL | mixed: `IllegalStateException: Failed to register 'filter unauthorizedFilter' ... Possibly already registered?` + `NullPointerException` (`FilterRegistration$Dynamic.setAsyncSupported`) |
| `JettyServletWebServerMvcIntegrationTests` | FAIL | `IllegalStateException: Failed to register 'servlet dispatcherServlet'/'servlet dispatcherRegistration' ... Possibly already registered?` |
| `JettyWebServerFactoryCustomizerTests` | HANG | no exception, ~3+ minutes between test-boundary log lines |
| `JettyServletWebServerServletContextListenerTests` | HANG | same shape |
| `JettyServletWebServerFactoryTests` | HANG | same shape (large test class, many `AbstractServletWebServerFactoryTests` methods) |

Representative `StackOverflowError` (`AutoConfigureWebServerJettyReactiveTests`):

```
Caused by: java.lang.StackOverflowError
  org.eclipse.jetty.server.handler.ContextHandler.doStart(ContextHandler.java:919)
  org.eclipse.jetty.ee11.servlet.ServletContextHandler.startContext(ServletContextHandler.java:1343)
  org.eclipse.jetty.ee11.servlet.ServletContextHandler.lambda$doStart$0(ServletContextHandler.java:1066)
  org.eclipse.jetty.server.handler.ContextHandler$ScopedContext.call(ContextHandler.java:1632)
  org.eclipse.jetty.server.handler.ContextHandler.doStart(ContextHandler.java:913)
  org.eclipse.jetty.ee11.servlet.ServletContextHandler.startContext(ServletContextHandler.java:1343)
  org.eclipse.jetty.ee11.servlet.ServletContextHandler.lambda$doStart$0(ServletContextHandler.java:1066)
  org.eclipse.jetty.server.handler.ContextHandler$ScopedContext.call(ContextHandler.java:1632)
  org.eclipse.jetty.server.handler.ContextHandler.doStart(ContextHandler.java:913)
  ... (repeats for thousands of frames)
```

Representative duplicate-registration (`JettyServletWebServerMvcIntegrationTests`):

```
Caused by: java.lang.IllegalStateException: Failed to register 'servlet dispatcherServlet' on the servlet context. Possibly already registered?
  org.springframework.boot.web.servlet.DynamicRegistrationBean.register(DynamicRegistrationBean.java:123)
  org.springframework.boot.web.servlet.RegistrationBean.onStartup(RegistrationBean.java:52)
  ...
  org.eclipse.jetty.ee11.webapp.WebAppContext.configure(WebAppContext.java:486)
  org.eclipse.jetty.ee11.webapp.WebAppContext.startContext(WebAppContext.java:1285)
  org.eclipse.jetty.ee11.servlet.ServletContextHandler.lambda$doStart$0(ServletContextHandler.java:1066)
  org.eclipse.jetty.server.handler.ContextHandler$ScopedContext.call(ContextHandler.java:1632)
```

Full logs (`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/`):
- `module_spring-boot-jetty.org.springframework.boot.jetty.autoconfigure.AutoConfigureWebServerJe-a54efb36e6cb.out.log` (reactive, `StackOverflowError`)
- `module_spring-boot-jetty.org.springframework.boot.jetty.autoconfigure.AutoConfigureWebServerJe-58885664b6d6.out.log` (servlet, "already registered")
- `module_spring-boot-jetty.org.springframework.boot.jetty.autoconfigure.JettyServerPropertiesTests.out.log`
- `module_spring-boot-jetty.org.springframework.boot.jetty.autoconfigure.metrics.JettyMetricsAuto-bd3a9a354a24.out.log`
- `module_spring-boot-jetty.org.springframework.boot.jetty.autoconfigure.reactive.JettyReactiveWe-d8962aeecfae.out.log`
- `module_spring-boot-jetty.org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebS-1d700487c5b2.out.log`
- `module_spring-boot-jetty.org.springframework.boot.jetty.reactive.JettyReactiveWebServerFactoryTests.out.log`
- `module_spring-boot-jetty.org.springframework.boot.jetty.servlet.JettyServletWebServerMvcIntegrationTests.out.log`
- `module_spring-boot-jetty.org.springframework.boot.jetty.autoconfigure.JettyWebServerFactoryCustomizerTests.err.log` (HANG)
- `module_spring-boot-jetty.org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebS-5e972da7fd0d.err.log` (HANG)
- `module_spring-boot-jetty.org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests.err.log` (HANG)

## Root cause (confirmed via decompiled Jetty bytecode + CratonVM source)

**This is a genuine CratonVM lambda-dispatch bug, verified end-to-end against
the real `jetty-server-12.1.8.jar` / `jetty-ee11-servlet-12.1.8.jar` bytecode
(`javap -v`, from `~/.gradle/caches/modules-2/files-2.1/org.eclipse.jetty/...`)
and CratonVM's own `try_lambda_dispatch` in
`vm/src/runtime/interpreter.rs`.**

### The real (intended) Jetty control flow

Both `org.eclipse.jetty.server.handler.ContextHandler` and its subclass
`org.eclipse.jetty.ee11.servlet.ServletContextHandler` independently declare
a **private, identically-named, identically-shaped** synthetic method:

```
private void lambda$doStart$0() throws java.lang.Exception;
```

(confirmed via `javap -v` — both are `ACC_PRIVATE`, descriptor `()V`). Each
class's own `doStart()`/`startContext()` calls `this::lambda$doStart$0` via a
`LambdaMetafactory` call site whose bootstrap method handle is
`REF_invokeVirtual <OwnClass>.lambda$doStart$0:()V` — i.e. **kind 5**
(`InvokeVirtual`), even though the target is `private`. This is standard,
correct javac output: per JVMS 5.4.3.3/6.5, `invokevirtual` (and the
`REF_invokeVirtual` `MethodHandle` kind used for `this::privateMethod`
references) resolves a **private** method statically to its exact declaring
class — there is no vtable slot for a private method, so it can never be
overridden and the "virtual" tag is cosmetic.

The real startup sequence for a `ServletContextHandler`/`WebAppContext`
instance is:

1. `ServletContextHandler.doStart()` calls `_context.call(this::lambda$doStart$0, null)` — correctly resolves to **`ServletContextHandler.lambda$doStart$0()`** (same class, trivially correct).
2. That method calls `this.startContext()` — a genuine virtual call, correctly polymorphic (dispatches to `WebAppContext.startContext()` when the receiver is a `WebAppContext`).
3. `ServletContextHandler.startContext()` (or `WebAppContext.startContext()`, which eventually calls `super.startContext()`) explicitly does `invokespecial ContextHandler.doStart()` — i.e. `super.doStart()` — at `ServletContextHandler.java:1343` (confirmed via `javap -c -l`: bytecode offset 156, `invokespecial #1000 // Method org/eclipse/jetty/server/handler/ContextHandler.doStart:()V`). `this` is still the original `ServletContextHandler`/`WebAppContext` instance; only the *method body being executed* changes.
4. Now running inside `ContextHandler.doStart()`'s own body (line 913, confirmed via `javap -c -l` on `ContextHandler.class`: `_context.call(this::lambda$doStart$0, null)`), the method-handle target is `REF_invokeVirtual ContextHandler.lambda$doStart$0:()V` — a **different, unrelated** private method that belongs to `ContextHandler`, not `ServletContextHandler`. Per real JVM semantics this must resolve to exactly `ContextHandler.lambda$doStart$0()`, which does `invokespecial Handler$Wrapper.doStart()` and terminates normally (starts the handler tree, no further recursion).

### The CratonVM bug

`try_lambda_dispatch` (`vm/src/runtime/interpreter.rs`, the
`MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface` arm,
~line 21171–21349) treats **every** `REF_invokeVirtual`-kind lambda impl
handle as genuinely polymorphic: it resolves `receiver_class` from the
**runtime class of `this`** (line 21263–21271: `class_manager.read().get_class(rcv_class_id)`)
and dispatches `member_name`/`descriptor` by name starting from that runtime
class (`invoke_on_class_shared`/`invoke_or_native`, lines 21312–21350) — it
never checks whether the target method is `private`. `impl_handle.class_name`
(the class the bootstrap actually recorded — `ContextHandler` in step 4
above) is consulted only as a **post-hoc fallback**, and only when the first
attempt raises a `NoSuchMethodError` naming exactly `(receiver_class,
member_name)` (lines 21370–21390) — which does not happen here, because the
lookup *succeeds*, just against the wrong method.

So in step 4, with `this` actually a `ServletContextHandler`/`WebAppContext`
instance, CratonVM resolves "`lambda$doStart$0`, descriptor `()V`" by walking
the **receiver's own class hierarchy** — and finds `ServletContextHandler`'s
own private `lambda$doStart$0()` (the wrong one, because `ServletContextHandler`
happens to define an identically-named-and-shaped private method for its own,
unrelated purpose) before ever considering `ContextHandler`'s. It invokes
that instead of `ContextHandler.lambda$doStart$0()`.

`ServletContextHandler.lambda$doStart$0()` calls `this.startContext()` —
**again** — which (for `WebAppContext`) re-runs `configure()`/`callInitializers()`
(re-registering every servlet/filter — the `"Possibly already registered?"`/
`FilterRegistration$Dynamic` NPE shapes) and then calls
`invokespecial ContextHandler.doStart()` **again** — landing back at step 4,
which mis-dispatches to `ServletContextHandler.lambda$doStart$0()` again.
This is the infinite loop visible in the `StackOverflowError` trace above,
with the exact repeating 4-frame cycle (`doStart:913` → `ScopedContext.call:1632`
→ `ServletContextHandler.lambda$doStart$0:1066` → `ServletContextHandler.startContext:1343`)
matching precisely what this mis-dispatch predicts.

**Why three different outward shapes:**
- **Plain `ServletContextHandler`** (reactive path, `JettyReactiveWebServerFactory`, no `WebAppContext`/no double servlet-registration guard triggered before exhausting the stack) → nothing stops the loop early → `StackOverflowError`.
- **`WebAppContext`-based** (regular servlet path) → the second run of `configure()`/`callInitializers()` hits Spring's own duplicate-registration guard (`DynamicRegistrationBean.register`) or a `null` `FilterRegistration.Dynamic` (servlet-spec-mandated `null` return for an already-registered name) after only 1–2 extra iterations, throwing a catchable exception instead of exhausting the stack.
- **HANG classes**: same recursive-restart pattern, but each iteration does enough real work (or the specific test scenario avoids both the stack-limit and the duplicate-registration guard for many iterations) that the cumulative time across the class's several/many `@Test` methods exceeds the harness's wall-clock timeout before either terminating condition fires. `JettyServletWebServerFactoryTests` in particular is a large `AbstractServletWebServerFactoryTests`-derived class with dozens of test methods, each independently paying the multi-minute cost. **This explanation for the HANG shape is a hypothesis** — the HANG logs show only sparse (multi-minute-apart) GC-guard WARN lines, not a live stack trace, so it is not independently confirmed that these three specific classes hit *this* mechanism rather than a different slow path; it is the most consistent explanation given the shared module, shared symptom family, and shared timing profile (FAIL cases in the same module complete their `StackOverflowError` in ~7-15s per test, and these HANG classes have many more test methods than the single/triple-test FAIL classes). All three HANG `.out.log` files are completely empty (0 bytes) — no Spring Boot banner, no `SBRUNNER_RESULT` — which was checked against the unrelated, already-filed [`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md) (a different HANG cluster with the same "zero JUnit output" outer shape); these three were **not** added there because their `.err.log` warning cadence is sparse (multi-minute gaps) rather than that cluster's steady sub-2s rhythm — a different, sparser pattern more consistent with slow-but-progressing recursion than a tight livelock. Not fully ruled out either way.

### Why this isn't the already-fixed `wrong-receiver-virtual-dispatch-corruption-cluster`

That cluster (`docs/internal/fixed-suite-bugs/wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`,
fixed today, 2026-07-17) is a **construction-time object-layout defect**: a
synthetic `Socket` allocated with too few fields, later read via real
bytecode. This bug is different in kind: no malformed object is involved:
`this` is a perfectly well-formed `ServletContextHandler`/`WebAppContext`
instance. The defect is purely in **lambda/method-handle dispatch logic**
incorrectly treating a `private`-method `REF_invokeVirtual` handle as
subject to receiver-class-driven polymorphism, and is unrelated to that
cluster's Case 1 or Case 2 mechanisms.

## What would confirm/refine this

A standalone repro with two classes replicating the shape (superclass and
subclass each declaring a private `foo()` invoked via `this::foo` from a
method the subclass calls via `super.method()` on the superclass) would
directly demonstrate the mis-dispatch without needing Jetty at all. The fix
direction: in the `MethodHandleKind::InvokeVirtual | InvokeInterface` arm of
`try_lambda_dispatch` (`vm/src/runtime/interpreter.rs` ~21171), check whether
the resolved impl method (looked up via `impl_handle.class_name` +
`member_name` + `descriptor`) is `ACC_PRIVATE`; if so, dispatch directly and
exclusively on `impl_handle.class_name` (matching the `MethodHandleKind::InvokeSpecial`
arm's existing "dispatch on the declaring class, no virtual lookup" behavior
just below it, ~line 21395), bypassing the receiver-class-driven resolution
entirely for that case.

## Affected classes

| Module | Class | Status |
|---|---|---|
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.AutoConfigureWebServerJettyReactiveTests` | FAIL |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.AutoConfigureWebServerJettyServletTests` | FAIL |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.JettyServerPropertiesTests` | FAIL |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.metrics.JettyMetricsAutoConfigurationTests` | FAIL |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.reactive.JettyReactiveWebServerAutoConfigurationTests` | FAIL |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerAutoConfigurationTests` | FAIL |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.JettyWebServerFactoryCustomizerTests` | HANG |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.reactive.JettyReactiveWebServerFactoryTests` | FAIL |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerServletContextListenerTests` | HANG |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.servlet.JettyServletWebServerMvcIntegrationTests` | FAIL |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` | HANG |
