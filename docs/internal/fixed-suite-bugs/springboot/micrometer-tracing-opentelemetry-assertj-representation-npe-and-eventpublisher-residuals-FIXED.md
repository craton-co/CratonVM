# ✅ FIXED — `module/spring-boot-micrometer-tracing-opentelemetry`: AssertJ `WritableAssertionInfo.representation` NPE (broad, masked real failures) + OTel event-publisher residual

**Status: RESOLVED 2026-07-26.** Both residuals are closed and the whole module
is green (9/9 classes, 71/71 tests PASS, JIT on).

| Residual | Root cause | Fixed by |
| --- | --- | --- |
| 1 — AssertJ `representation` NPE masking every failing `String`/`CharSequence` assertion, suite-wide | CratonVM's own `native_assertj_lightweight_comparable_assert` shim hand-built `AbstractAssert` instead of running its constructor, leaving `WritableAssertionInfo.representation` null | `fdc852f558` (2026-07-26 14:58 UTC, separate session) |
| 1b — the *real* failure the NPE had been hiding: `OpenTelemetryBaggagePropagationIntegrationTests.shouldSetEntriesToMdcFromSpanWithBaggage[1] OTEL_DEFAULT`, MDC `traceId` null | loader-blind lambda-impl resolution in the native-callback dispatcher (see below) | this session, `vm/src/vm/vm_exec.rs` |
| 2 — `OpenTelemetryTracingAutoConfigurationTests.shouldPublishEventsWhenContextStorageIsInitializedEarly`, `listener.events` empty | **same** root cause as 1b | this session, same one-line change |

Verification worktree: `/data/data/wt-assertjnpe-20260726` (Linux Azure host),
branch `fix/assertj-representation-npe-20260726`, binary
`/data/data/assertjnpe/cvm-ajnpe-fix1`.

---

## Residual 1 — AssertJ `representation` NPE — FIXED by `fdc852f558`, not by this session

The original investigation (2026-07-23/24) spent two sessions and ~15 iterative
repros on this and could not isolate it, because every hypothesis assumed the
Java bytecode was running. **It wasn't.** `Assertions.assertThat(String)` was
being intercepted by a CratonVM *native shim*,
`native_assertj_lightweight_comparable_assert`, which builds an `AbstractAssert`
by hand rather than executing the real constructor and left the allocated
`WritableAssertionInfo` with a null `representation` — a state the real
constructor makes impossible (it routes through
`useRepresentation(...)`/`Objects.requireNonNull`).

That single fact explains every otherwise-baffling observation in the original
doc, and they should be read as *evidence for a native interception*, not
against it:

- Only the **real, precompiled** `Assertions`/`AssertionsForClassTypes` classes
  reproduced it; identical hand-written bytecode never did — because the shim
  is keyed on those exact class/method names.
- `new StringAssert("x")` called **directly** worked — it doesn't go through
  the shimmed static entry point.
- The classpath-shadowed instrumented `WritableAssertionInfo` constructor's
  `println`s "never fired" — correct, and *not* an artifact of the shadowing
  technique: the constructor genuinely never ran.
- `--nojit` and zero-GC reproduction ruled out JIT and moving-GC — correctly,
  since the bug was in a native builtin.

**Bisect (this session, using the doc's own 15-line repro):** `git bisect run`
over the 142 `dev` commits between `26b66dc76` (2026-07-26 03:12 -0300, the
state the stale Windows binary was built from) and `3be41785e`, 6 build+test
steps, pinned `fdc852f558` as the first fixed commit — matching its own commit
message exactly. The bug is **platform-independent**: a Linux binary built at
`26b66dc76` reproduces the NPE with byte-identical symptoms to the Windows
binary, so the earlier "only reproduces on Windows"-looking evidence was purely
a stale-binary artifact.

**Regression probe** (`Repro2.java`, kept at
`/data/data/assertjnpe/src/Repro2.java`) — all 8 assertion shapes the two
sessions between them saw fail, checked JIT-on and `--nojit`, all now produce a
proper `AssertionError`:

| assertion | old | now |
| --- | --- | --- |
| `assertThat(String).isEqualTo` | NPE | AssertionError |
| `assertThat(String).isNull` | NPE | AssertionError |
| `assertThat(String).contains` (the `MessageFormatter.asText` call site) | NPE (`"p" is null`) | AssertionError |
| `assertThat(String).as(...).isEqualTo` | NPE | AssertionError |
| `assertThat(List).contains` | ok | ok |
| `assertThat(String).satisfiesAnyOf(...)` | NPE | AssertionError |
| `assertThat((Object)String).isEqualTo` | ok | ok |
| `assertThat(StringBuilder).isEqualTo` | ok | ok |

**Anyone still seeing this NPE has a binary older than `fdc852f558` — rebuild.**
That specifically applies to `C:\craton\cratonvm\target\release\cratonvm.exe`,
which as of this session was built 2026-07-26 03:17 -0300 and still reproduces
it.

The 3 `module/spring-boot-jetty` methods the original doc listed as collateral
(`JettyReactiveWebServerFactoryTests.specificIPAddressNotReverseResolved`,
`JettyServletWebServerFactoryTests.specificIPAddressNotReverseResolved` /
`specificIPAddressWithSslIsNotReverseResolved`, and the 9 parameterized
`sessionCookieSameSiteAttributeCanBeConfiguredAndOnlyAffectsSessionCookies*`
cases) all **PASS** now — confirmed on both the pre- and post-fix binaries of
this session, i.e. closed by `fdc852f558` as predicted.

---

## Residual 1b + 2 — one root cause: loader-blind lambda-impl resolution in the native-callback dispatcher

With the NPE gone, the module dropped from 5+1 failing tests to exactly 2, and
both turned out to be the same VM bug:

```
OpenTelemetryBaggagePropagationIntegrationTests
  shouldSetEntriesToMdcFromSpanWithBaggage[1] autoConfig = OTEL_DEFAULT
  => AssertionFailedError: [MDC[traceId]] expected: "f7276d…" but was: null

OpenTelemetryTracingAutoConfigurationTests
  shouldPublishEventsWhenContextStorageIsInitializedEarly
  => AssertionError: Expecting actual not to be empty      (listener.events)
```

### Symptom shape (the clue that cracked it)

Only the **first** `@ForkedClassPath` test in the JVM failed; every later one
passed. Running each method alone reproduced it deterministically in ~15s
(`SbRunner2`, kept at `/data/data/assertjnpe/src/SbRunner2.java` — a `SbRunner`
variant taking `<fqcn> [method] [paramTypes]`).

A classpath overlay of the test class printing loader identities gave the
decisive line:

```
[T before-withSpan OTEL_DEFAULT]
   Context.class=…Context@cl18462          ContextStorage.class=…ContextStorage@cl18462
   storageGet=…$Wrapper$Storage@cl18462
   current=io.opentelemetry.context.ArrayBasedContext@cl203   <-- WRONG LOADER
   currentIfaces=[io.opentelemetry.context.Context@cl203]
```

Everything the test touched directly was the child loader's copy, yet
`Context.current()` handed back an **Application-loader** `Context`.

### Root cause

`OpenTelemetryEventPublisherBeansApplicationListener.onApplicationEvent` builds
its wrappers with

```java
applicationContext.getBeansOfType(EventPublisher.class, true, false).values()
    .stream()
    .map(EventPublishingContextWrapper::new)   // <-- here
    .toList();
```

Under `@ForkedClassPath`'s `ModifiedClassPathClassLoader` the listener class is
child-loader-defined, so that constructor reference must resolve
`EventPublishingContextWrapper` through the child loader. It didn't:

```
[OTELCL-SUPER] defining=io/micrometer/tracing/otel/bridge/EventPublishingContextWrapper$1
               loader_id=Application  super=io/opentelemetry/context/ContextStorage
               PROBE-HIT=ClassId(960)          <-- Application ContextStorage
[OTELCL-STATIC] io/opentelemetry/context/ArrayBasedContext.root
               from=io/opentelemetry/context/ContextStorage
               cur_cid=ClassId(960) cur_loader=Application
```

A Rust backtrace on the fresh Application-loader definition named the exact
path:

```
ClassManager::load_class(io/micrometer/tracing/otel/bridge/EventPublishingContextWrapper) FRESH
  1: invoke_virtual                     vm/src/vm/vm_exec.rs:9256
  2: invoke_deferred_stream_lambda      native-collections/src/lib.rs:13978
  3: stream_process_chain               native-collections/src/lib.rs:14078
  …
  9: native_stream_to_list              native-collections/src/lib.rs:17462
```

`NativeContextImpl::invoke_virtual`'s lambda dispatcher used the **passive**
`lambda_impl_dispatch_override`, which only *reads* an already-populated
initiating-resolution cache. Its sibling dispatcher in `interpreter.rs`
(`try_lambda_dispatch`) had long since been switched to the **driven** variant
`lambda_impl_dispatch_override_driven`, which actually invokes the host loader's
`loadClass` on a cold miss — that fix was never mirrored onto this copy.

This dispatcher is the one used when a **native** helper calls back into a Java
lambda — above all `native-collections`' Stream pipeline. A method reference
evaluated only from inside such a native callback can be the very *first*
reference to its impl class from an isolated loader's namespace, so the passive
lookup misses and the `NewInvokeSpecial`/`Invoke*` arms fall through to the
loader-BLIND `class_manager.load_class(name)` / `invoke_or_native(name)`.

The consequence cascaded: the Application-loader
`EventPublishingContextWrapper$1` (an anonymous `ContextStorage`) inherited the
**Application** `io.opentelemetry.context.ContextStorage`, whose default
`root()` returns the Application `ArrayBasedContext.ROOT`. `Context.current()`
therefore returned a foreign-loader `Context`, `makeCurrent()` dispatched
`Context.makeCurrent`'s default body in the Application namespace and attached
to the Application `ContextStorage` — which carries no event-publishing wrapper.
No `EventPublisher` events fired: MDC `traceId` stayed null (Residual 1b) and
`OtelEventListener.events` stayed empty (Residual 2). From the second test
onwards the child loader had defined its own copy for other reasons, the passive
cache hit, and everything worked — hence "only the first test fails".

### Fix

`vm/src/vm/vm_exec.rs` — use `lambda_impl_dispatch_override_driven` (passing
`self.thread`) instead of the passive `lambda_impl_dispatch_override`, matching
`interpreter.rs`'s dispatcher.

> **Generalizable lesson (again):** loader-blind resolution used where a
> loader-aware one is needed. This is the same defect family as
> `native_class_get_nest_members`, `resolve_fast_path_class_id`'s
> global-first fallback, and the earlier `lambda_impl_dispatch_override_driven`
> fix itself. When a fix like that lands, **grep for other call sites of the
> passive helper** — this one sat unfixed for weeks purely because the VM has
> two parallel lambda dispatchers.

---

## Verification

`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1` (via `/snap/bin/pwsh`)
against `-SpringBootRoot /data/data/springboot-jsonreader-deprecation-20260718`,
JIT on:

**`module/spring-boot-micrometer-tracing-opentelemetry` — 9/9 PASS**

| class | tests |
| --- | --- |
| `OpenTelemetryBaggagePropagationIntegrationTests` | 8 ✅ (was 1 FAIL after the NPE fix, 5 FAIL before it) |
| `OpenTelemetryTracingAutoConfigurationTests` | 36 ✅ (was 1 FAIL) |
| `CompositeTextMapPropagatorTests` | 5 ✅ |
| `OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests` | 1 ✅ |
| `OpenTelemetryTracingPropertiesTests` | 1 ✅ |
| `SpanExportersTests` | 2 ✅ |
| `SpanProcessorsTests` | 2 ✅ |
| `otlp.OtlpTracingAutoConfigurationTests` | 24 ✅ |
| `otlp.OtlpTracingAutoConfigurationIntegrationTests` | 3 ✅ |
| `zipkin.ZipkinWithOpenTelemetryTracingAutoConfigurationTests` | 13 ✅ |

**`module/spring-boot-jetty` — no regression**, byte-for-byte the same outcome
on the fix binary and the pre-fix baseline binary. 9 of 13 classes PASS; the 4
non-PASS are all pre-existing and unrelated:

| class | outcome | cause |
| --- | --- | --- |
| `SslServerCustomizerTests` | CRASH (both binaries) | pre-existing |
| `autoconfigure.servlet.JettyServletWebServerServletContextListenerTests` | 2/2 FAIL (both) | `org.mockito.exceptions.misusing.NotAMockException` |
| `reactive.JettyReactiveWebServerFactoryTests` | 35 tests, 1 FAIL (both) | `whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade` awaitility 30s timeout |
| `servlet.JettyServletWebServerFactoryTests` | 113 tests, 3 FAIL (both) | 2 × `/tmp` directory-permission `IllegalStateException` (host environment) + the same graceful-shutdown assertion |

> Harness note: `JettyServletWebServerFactoryTests` shows as `HANG` at the 600 s
> timeout when run with `-Parallel 4` alongside the other Jetty web-server
> classes on this 16-core host; run alone it completes in ~200 s with exactly
> the baseline's 3 failures. Don't read that HANG as a VM bug.

### Reproduce

```bash
# 15-line AssertJ NPE probe (sub-second) — expects a normal AssertionError now
cratonvm --java-home <jdk25> --Xmx 512m -cp "<out>:<assertj-core-3.27.7.jar>" Repro2
```

```bash
# the two OTel classes
pwsh -NoProfile -File apps/spring-boot-suite-runner/run-spring-boot-suite.ps1 \
  -Vm craton -Exe <exe> -JdkHome /home/victor/jdk25 \
  -SpringBootRoot /data/data/springboot-jsonreader-deprecation-20260718 \
  -ClassList <tsv with the 2 classes> -RunName <name> -Parallel 2 -TimeoutSec 400
```

Single method, no harness (fastest loop, ~15 s):

```bash
cratonvm --java-home /home/victor/jdk25 --Xmx 2g --stack-dump-on-timeout 0 \
  -cp "<probe-dir>:$(cat cratonvm-test-cp.txt)" SbRunner2 \
  org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryBaggagePropagationIntegrationTests \
  shouldSetEntriesToMdcFromSpanWithBaggage \
  'org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryBaggagePropagationIntegrationTests$AutoConfig'
```
