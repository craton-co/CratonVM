# `printStackTrace()` never emits a "Suppressed:" section — `WebServer` stop/destroy-failure tests can't see it

**Status: OPEN — found 2026-07-17**

**Update 2026-07-17 (same-day parallel triage): confirmed root cause found, superseding the Mockito-restub hypothesis below.**
Read `native-builtins/src/lang_misc.rs` directly. `ServletWebServerApplicationContext.refresh()`
(`apps/spring-boot/module/spring-boot-web-server/src/main/java/org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext.java:140-158`)
does exactly what the test expects on the Java side:

```java
public final void refresh() throws BeansException, IllegalStateException {
    try {
        super.refresh();
    }
    catch (RuntimeException ex) {
        WebServer webServer = this.webServer;
        if (webServer != null) {
            try {
                webServer.stop();
                webServer.destroy();
            }
            catch (RuntimeException stopOrDestroyEx) {
                ex.addSuppressed(stopOrDestroyEx);
            }
        }
        throw ex;
    }
}
```

i.e. a stop/destroy failure is attached via `Throwable.addSuppressed()`,
which real HotSpot's `Throwable.printStackTrace()` renders as a nested
`Suppressed: ...` block (via `Throwable.printEnclosedStackTrace`). AssertJ's
`withStackTraceContaining(...)` calls `printStackTrace(PrintWriter)`
internally and checks the resulting text — so the suppressed exception's
message being present in the printed trace is exactly what the test relies
on, regardless of whether Mockito's stub actually fired.

**CratonVM's native `printStackTrace()` never prints suppressed exceptions
at all.** `native-builtins/src/lang_misc.rs`, `collect_throwable_chain_lines`
(~line 1181-1205) — the single shared body for all 3
`printStackTrace()`/`printStackTrace(PrintStream)`/`printStackTrace(PrintWriter)`
native overloads (registered at ~line 2044-2151) — only emits the
receiver's own header+frames, then walks the `cause` chain via `Caused by:`
lines (`throwable_cause`/depth-guarded loop, lines 1187-1203). It never
reads the `suppressedExceptions` field (index 4 in the real-JDK `Throwable`
layout, confirmed by the doc comment on the nearby `addSuppressed` native at
line ~1242) or emits any `Suppressed:` section. This is a native
reimplementation of `printStackTrace()` (not real bytecode), so this gap is
unconditional — it applies regardless of whether `addSuppressed` itself, or
the Mockito stub that triggers the secondary exception, work correctly.
This fully explains the symptom without requiring the (unconfirmed) Mockito
re-stub theory: even if `webServer.stop()`/`destroy()` throw exactly as
stubbed and `addSuppressed` correctly records it, the resulting suppressed
exception can never appear in `printStackTrace()`'s output, so
`withStackTraceContaining("WebServer has failed to stop")` can never pass.

**What to fix:** `collect_throwable_chain_lines` needs a suppressed-exception
pass — after (or interleaved with) the cause chain, read the receiver's
`suppressedExceptions` list (real-JDK field index 4) and, for each entry,
emit an indented `Suppressed: <header>` block plus that exception's own
frames/cause chain, matching real `Throwable.printEnclosedStackTrace`'s
format closely enough for substring assertions like AssertJ's
`withStackTraceContaining` to find the expected text.

The original (now superseded) investigation pass is preserved below.

## Symptom

| Module | Class | Failures |
|---|---|---:|
| `module/spring-boot-web-server` | `ReactiveWebServerApplicationContextTests` | 2 |
| `module/spring-boot-web-server` | `ServletWebServerApplicationContextTests` | 2 |

Both tests use the identical pattern: stub a `WebServer` mock's `.stop()`/`.destroy()` to throw, applied *after* the mock is already in use elsewhere in the context, inside a bean-supplier lambda that runs during `context.refresh()`:

```java
willThrow(new RuntimeException("WebServer has failed to stop")).willCallRealMethod()
    .given(this.context.getWebServer())
    .stop();
```

Expected: a `BeanCreationException` whose stack trace contains `"WebServer has failed to stop"` (or `"...to destroy"`). Actual: the exception chain only contains `Caused by: java.lang.RuntimeException: Fail refresh` — no "WebServer has failed to stop/destroy" anywhere. The AssertJ `withStackTraceContaining(...)` assertion fails.

Test source: `apps/spring-boot/module/spring-boot-web-server/src/test/java/org/springframework/boot/web/server/reactive/context/ReactiveWebServerApplicationContextTests.java:140-171` (and the servlet-context sibling test, same structure).

Logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard*/logs/module_spring-boot-web-server.org.springframework.boot.web.server.reactive.context.ReactiveWeb-2bc7f172ab67.out.log` (lines 17-113, 204-302)
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard*/logs/module_spring-boot-web-server.org.springframework.boot.web.server.servlet.context.ServletWebSe-8b4b7fae3fac.out.log` (lines 17-...)

## Root cause

**Hypothesis, not confirmed.** The stub is applied to a mock *after* it's already in use elsewhere in the context; under CratonVM the late re-stub doesn't take effect (`stop()`/`destroy()` just runs the mock's default/real behavior instead of throwing), so the code path that's supposed to catch the stop/destroy failure and prepend `"WebServer has failed to stop/destroy"` to the exception message never fires.

No existing doc covers this (`grep -rl "willCallRealMethod\|WebServer has failed to stop"` across `docs/internal/fixed-suite-bugs/`, `docs/internal/springboot/`, `docs/known-issues/springboot/` → zero hits). Likely the same general family as the already-fixed `http-client-simpleclienthttpresponsetests-mockito-dispatch-bugs-FIXED.md` and `kafka-bug-B-mockito-mockstatic-mock-dispatch.md`, but **not the same exact stubbing style** — those are `mockStatic`; this is BDDMockito `willThrow().willCallRealMethod().given(mock).method()` re-stubbing an already-created instance mock mid-test. Needs a source-level look into CratonVM's Mockito/byte-buddy proxy dispatch (how a mock's stubbing table is consulted on each invocation vs. cached at creation time) to confirm.

## Affected classes

- `module/spring-boot-web-server` | `ReactiveWebServerApplicationContextTests`
- `module/spring-boot-web-server` | `ServletWebServerApplicationContextTests`
