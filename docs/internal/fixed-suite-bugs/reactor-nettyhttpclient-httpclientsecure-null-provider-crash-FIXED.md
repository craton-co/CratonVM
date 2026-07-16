# `reactor/netty/http/client/HttpClientSecure.<clinit>` NPE (`"provider"`) crashes the whole test process

**Status: FIXED (2026-07-16)**

## Resolution

Native SSLContext client-session support prevents Netty HTTP/2 setup from falling through to a null real-JDK SSLContextSpi. The closure also preserves JKS entry-password handling, server fatal-alert delivery, real embedded-Tomcat startup, and primitive MethodHandle return unboxing.

- JIT off: both affected classes passed, 32/32 and 33/33 (`reactor-nettyhttpclientsecure-r2-final-jitoff-confirm`).
- JIT on: both affected classes passed, 32/32 and 33/33 (`reactor-nettyhttpclientsecure-r2-final-jiton`).

## Symptom

Two classes in `module/spring-boot-http-client` CRASH (process exit, zero
tests recorded) with the byte-for-byte identical failure. Filenames are
truncated by the runner's own log-naming scheme, not by any encoding issue:

- `org.springframework.boot.http.client.reactive.ReactorClientHttpConnectorBuilderTests`
  — `.suite\results\crashfail-20260714\shard5\logs\module_spring-boot-http-client.org.springframework.boot.http.client.reactive.Reacto-be41cc4b2a1e.err.log`
- `org.springframework.boot.http.client.ReactorClientHttpRequestFactoryBuilderTests`
  — `.suite\results\crashfail-20260714\shard5\logs\module_spring-boot-http-client.org.springframework.boot.http.client.ReactorClientHt-2c1de960da8a.err.log`

Both `.out.log` files are empty — the crash happens before any JUnit output
is flushed. The raw `.err.log` (ANSI color codes stripped for readability;
content otherwise verbatim) tail for the connector-builder class:

```
WARN cratonvm_vm::vm::vm_exec: Missing native method in real-JDK mode method=io/netty/handler/codec/quic/Quiche.quiche_version()Ljava/lang/String;
WARN cratonvm_vm::vm::vm_exec: Missing native method in real-JDK mode method=io/netty/handler/codec/quic/QuicheNativeStaticallyReferencedJniMethods.afInet()I
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError class=reactor/netty/http/client/HttpClientSecure cause=java/lang/NullPointerException provider
WARN cratonvm_vm::vm::vm_util:   [CLINIT-TRACE 0] at SbRunner.main (SbRunner.java:36) bci=133
WARN cratonvm_vm::vm::vm_util:   [CLINIT-TRACE 1] at org/junit/platform/launcher/core/SessionPerRequestLauncher.execute (SessionPerRequestLauncher.java:67) bci=13
   ... (JUnit launcher/engine frames, all inside SbRunner.main's call graph) ...
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error: no class def found: reactor/netty/http/client/HttpClientSecure
[cratonvm] main-vm run() Err (debug): Error in thread "main" linkage error: no class def found: reactor/netty/http/client/HttpClientSecure
```

The originally-reported summary of this cluster paraphrased the WARN line as
`<clinit> failed ??????? wrapping in ExceptionInInitializerError class=r` —
that truncation/mangling was an artifact of an earlier `grep`/terminal
encoding step, **not** something CratonVM itself printed. The full raw line
reads exactly `<clinit> failed — wrapping in ExceptionInInitializerError
class=reactor/netty/http/client/HttpClientSecure cause=java/lang/NullPointerException provider`
(the `—` em-dash is the only non-ASCII character; it round-trips fine once
ANSI codes are stripped).

Both classes fail on the exact same class (`HttpClientSecure`) with the exact
same cause — they only differ in which reactor-netty builder API they
exercise, both of which trigger `HttpClientSecure`'s static initializer as a
side effect of building a `Reactor` HTTP client (JDK 25, real-JDK mode,
`reactor-netty-http:1.3.5` / `reactor-netty-core:1.3.5` from
`~/.gradle/caches/modules-2/files-2.1/io.projectreactor.netty/...`).

## Root cause

### The NPE is real reactor-netty code failing an already-caught internal fallback

Decompiling `reactor/netty/http/client/HttpClientSecure.class` (`javap -p -c`,
extracted from the module's actual
`reactor-netty-http-1.3.5.jar`) shows the static initializer builds three
`SslProvider` fields, with the first two wrapped in their own internal
`try { ... } catch (Exception e) { ... = null; }` blocks (reactor-netty's own
graceful-degradation logic — this is by design, present in real Java too):

```
static {};
  0: SslProvider.builder().sslContext(Http2SslContextSpec.forClient()).build()
 16: astore_0                    // -> HTTP2_SSL_PROVIDER
 17: goto 23
 20: astore_1 (catch Exception)
 21: aconst_null
 22: astore_0                    // HTTP2_SSL_PROVIDER = null on failure
 23: putstatic HTTP2_SSL_PROVIDER
 27: Http3.isHttp3Available() ? build DEFAULT_HTTP3_SSL_PROVIDER : null   // also try/catch-guarded
 68: putstatic DEFAULT_HTTP3_SSL_PROVIDER
 71: SslProvider.defaultClientProvider()
 77: SslProvider.addHandlerConfigurator(that, HOSTNAME_VERIFICATION_CONFIGURER)
 80: putstatic DEFAULT_HTTP_SSL_PROVIDER
 83: getstatic HTTP2_SSL_PROVIDER            // <-- reads the (possibly null) value from pc 0-23
 89: invokestatic SslProvider.addHandlerConfigurator(HTTP2_SSL_PROVIDER, configurer)
 92: putstatic DEFAULT_HTTP2_SSL_PROVIDER
 95: return
Exception table:
  0  to 17  -> 20  Class java/lang/Exception   (guards the HTTP2_SSL_PROVIDER build)
  27 to 61  -> 64  Class java/lang/Exception   (guards the HTTP3 attempt)
  # NOTE: the pc 71-95 tail (DEFAULT_HTTP_SSL_PROVIDER / DEFAULT_HTTP2_SSL_PROVIDER
  # construction) has NO exception-table entry.
```

And `reactor/netty/tcp/SslProvider.addHandlerConfigurator` (decompiled from
`reactor-netty-core-1.3.5.jar`):

```
public static SslProvider addHandlerConfigurator(SslProvider provider, Consumer<...> handlerConfigurator) {
   0: aload_0
   1: ldc "provider"
   3: invokestatic Objects.requireNonNull:(Object, String)Object   // <-- matches cause exactly
   ...
```

This is an exact match for the observed exception: `NullPointerException:
provider`. The mechanism is:

1. Building `HTTP2_SSL_PROVIDER` (`SslProvider.builder().sslContext(Http2SslContextSpec.forClient()).build()`,
   pc 0-16) throws **some** exception inside CratonVM. Because this call is
   wrapped in reactor-netty's own `catch (Exception e)`, the throw is
   **silently swallowed** — CratonVM never logs it (it never reaches
   `<clinit>`-failure handling, since the enclosing `HttpClientSecure.<clinit>`
   hasn't failed yet at this point), leaving `HTTP2_SSL_PROVIDER = null`.
2. Execution continues normally into the unguarded tail (pc 71-95), which
   passes that null `HTTP2_SSL_PROVIDER` into
   `SslProvider.addHandlerConfigurator(null, ...)` at pc 89 — outside any
   `try`/`catch` in `HttpClientSecure.<clinit>` — which immediately NPEs on
   its own `Objects.requireNonNull(provider, "provider")` guard.
3. That **uncaught** NPE is what CratonVM's `<clinit>`-failure path
   (`vm/src/vm/vm_util.rs`, the `<clinit> failed — wrapping in
   ExceptionInInitializerError` `tracing::warn!` around line 1615-1620) sees,
   correctly wraps per JVMS §5.5, and correctly marks the class
   `InitializationError` (`finalize_init(..., ClassState::InitializationError)`,
   line 1540).

**What is not yet known**: the *first* exception (whatever
`SslProvider.builder().sslContext(Http2SslContextSpec.forClient()).build()`
throws inside CratonVM) is never logged anywhere, because reactor-netty's own
`catch (Exception e)` swallows it before it becomes a CratonVM
`<clinit>`-failure event. `Http2SslContextSpec.forClient()` itself is a thin
wrapper around `io.netty.handler.ssl.SslContextBuilder.forClient()`; the
actual work — and most likely failure point — happens in `SslProvider`'s own
internal `Builder.build()`, which constructs a real Netty `SslContext`
(ALPN negotiator selection, `OpenSsl.isAvailable()` probing, JDK
`SSLContext`/`SSLParameters` ALPN wiring). Given this worktree's existing
notes on CratonVM's TLS/JSSE stack having multiple incomplete areas (JCA
provider chain, rustls cipher-suite gaps, PKCS12/JSSE handshake fixes — see
Related below), the most likely culprit is one of those same gaps being hit
during HTTP/2 `SslContext` construction, but this has **not** been isolated
by this investigation — it would need either (a) a CratonVM debug patch to
log the swallowed exception before reactor's own catch, or (b) a minimal
standalone Java repro that calls
`SslProvider.builder().sslContext(Http2SslContextSpec.forClient()).build()`
directly under CratonVM with verbose tracing enabled.

### Why this crashes the whole process instead of failing one test

Real HotSpot almost certainly never takes the NPE branch at all — the HTTP2
`SslContext` build at pc 0-16 succeeds there (real JDK has full ALPN/OpenSSL
support), so `HTTP2_SSL_PROVIDER` is non-null when it reaches
`addHandlerConfigurator` at pc 89, and `HttpClientSecure.<clinit>` completes
normally. Reactor-netty's own `try`/`catch` around the HTTP2/HTTP3 builds is
clearly meant to make partial-capability environments degradation-tolerant
(e.g., no `netty-tcnative`/OpenSSL on the classpath) — but its own author
apparently did not anticipate the *un-guarded* pc 71-95 tail dereferencing a
value that the guarded pc 0-16 block can leave null. That is a latent gap in
reactor-netty itself, only reachable when the HTTP2 build fails — which is
exactly the condition CratonVM (for whatever underlying, still-unidentified
reason) triggers.

Per the `<clinit>`-failure policy documented at the top of
`vm/src/vm/vm_util.rs` (JVMS §5.5 default-strict propagation, `lenient_clinit()`
off by default), the wrapped `ExceptionInInitializerError` is intentionally
**not** swallowed — this part of CratonVM's behavior is correct and
JVMS-conformant. The trace shows `HttpClientSecure`'s `<clinit>` is first
triggered directly from `SbRunner.main` (`[CLINIT-TRACE 0] at SbRunner.main
(SbRunner.java:36) bci=133`), i.e. during JUnit-launcher setup/discovery
inside `SbRunner`'s own `main()`, not inside a JUnit-engine-wrapped test
method invocation. Because nothing in that call path catches the resulting
`Error`, it propagates all the way to `vm-cli/src/main.rs`'s top-level `run()`
handler (~line 3672-3682):

```rust
match run() {
    Ok(()) => { ... }
    Err(e) => {
        eprintln!("[cratonvm] main-vm run() returned Err: {e:#}");
        eprintln!("[cratonvm] main-vm run() Err (debug): {e:?}");
        let _ = std::io::stderr().flush();
        std::process::exit(1);
    }
}
```

— which prints the `linkage error: no class def found: ...` (the class is
now permanently `InitializationError`, so any subsequent use throws
`NoClassDefFoundError`, itself a subclass of `Error`/`LinkageError` and thus
legitimately propagated as-is per the `is_error` check at
`vm/src/vm/vm_util.rs` ~line 1543-1555) and exits the whole process with
`SBRUNNER_RESULT` never emitted — recorded by the suite runner as `CRASH`
with **zero** tests reported for the entire class, rather than the class
discovery step failing gracefully or (if reached from inside actual test
execution) a normal per-test JUnit failure.

This is the same general failure shape as
`applicationcontextrunnertests-lazy-cglib-classnotfound-crash.md` (an
uncaught `Error`/internal-error reaching `vm-cli`'s top-level handler and
`exit(1)`-ing the whole process instead of being contained to one test or
one discovery step) but a **different underlying mechanism** — that doc's
root cause is a CratonVM classloader gap misrouting a genuinely recoverable
`ClassNotFoundException` into an unrecoverable `InternalError`; this one is a
legitimately-thrown, correctly-propagated JVMS `Error` (`NoClassDefFoundError`
after a real `ExceptionInInitializerError`) whose only failure of isolation
is that `SbRunner`/the JUnit launcher setup path that triggers it (at
`SbRunner.main` bci=133, before entering JUnit's own per-test exception
handling) has no surrounding catch to turn "this one test class's HTTP client
builder is broken" into a contained per-class failure instead of an
uncontained process abort.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Category all -Jit on `
  -RefreshLists -Start 1 -Count 0 `
  -RunName crashfail-20260714-repro
```

or, once `all-tests.tsv`/`others.tsv` indices are known (`-ListOnly` first),
isolate either class individually with `-Start <n> -Count 1` per
`apps\spring-boot-suite-runner\run-spring-boot-suite.md`; both reproduce the
identical `HttpClientSecure` crash standalone in `module/spring-boot-http-client`,
independent of `-Parallel`.

## Related

- `docs\known-issues\springboot\applicationcontextrunnertests-lazy-cglib-classnotfound-crash.md`
  — same crash *shape* (uncaught error reaching `vm-cli/src/main.rs`'s
  top-level handler, `exit(1)`, zero tests recorded), different mechanism
  (that one is a misrouted recoverable `ClassNotFoundException`; this one is
  a legitimately-propagated `Error` whose root trigger is an unlogged,
  swallowed-then-resurfaced NPE three call-frames upstream).
- `reference_jca_synthetic_crypto_layers.md`,
  `reference_rustls_no_dhe_support.md`,
  `reference_pkcs12_tls_handshake_chain_20260702.md`,
  `reference_jsse_nio_sslengine_bytebuffer_hang.md` (session memory) — this
  worktree's existing catalog of CratonVM TLS/JSSE/JCA gaps; the still-unknown
  first exception swallowed by reactor-netty's own `catch (Exception e)` at
  `HttpClientSecure.<clinit>` pc 0-16 (building an HTTP/2 `SslContext` via
  `SslProvider.builder().sslContext(Http2SslContextSpec.forClient()).build()`)
  is plausibly one of these same known-incomplete areas, but has not been
  isolated by this investigation.
- `vm/src/vm/vm_util.rs` — `<clinit>` failure policy doc comment (top of
  file), the `tracing::warn!("<clinit> failed — wrapping in
  ExceptionInInitializerError")` call (~line 1615-1620), and the
  `Error`-vs-wrap branch (~line 1543-1555).
- `vm-cli\src\main.rs` (~line 3672-3682) — the top-level `run()` error
  handler that turns any uncaught `Err` into `eprintln!` + `exit(1)`.
- Reactor Netty upstream: `reactor.netty.http.client.HttpClientSecure`
  (`reactor-netty-http:1.3.5`) and `reactor.netty.tcp.SslProvider`
  (`reactor-netty-core:1.3.5`) — the unguarded `pc 71-95` tail of
  `HttpClientSecure.<clinit>` dereferencing a possibly-null
  `HTTP2_SSL_PROVIDER` is arguably a latent robustness gap in reactor-netty
  itself, only reachable when the guarded HTTP2 build genuinely fails (which
  it should not, and does not on real HotSpot, in this environment).
- The two `Missing native method in real-JDK mode` warnings immediately
  preceding the `<clinit>` failure in the logs
  (`io/netty/handler/codec/quic/Quiche.quiche_version()`,
  `QuicheNativeStaticallyReferencedJniMethods.afInet()`) are **not** the
  cause — those are gated behind `Http3.isHttp3Available()` inside the
  *second* try/catch block (pc 27-61, `DEFAULT_HTTP3_SSL_PROVIDER`), a
  separate code path from the unguarded pc 71-95 tail that actually NPEs;
  they are logged first only because of bytecode ordering, not because
  they're the throw site.
