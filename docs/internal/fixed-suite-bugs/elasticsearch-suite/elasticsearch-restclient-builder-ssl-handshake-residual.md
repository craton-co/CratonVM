# Elasticsearch RestClientBuilderIntegTests SSL handshake residual failures

Status: FIXED

Date observed: 2026-07-02

## Fix applied (2026-07-04)

High-confidence root cause and deterministic fix were implemented for this issue:

- `../../../../native-builtins/src/t27_tls.rs` now keys `ctx_identity_table()` and
  `ctx_trust_roots_table()` by a GC-stable object key derived from
  `NativeContext::identity_hash_code` (with generation tracking), rather than
  the unstable `ObjectRef` debug-text hash.
- `../../../../native-builtins/src/net_phase_e.rs` and related `t27_tls` call sites now pass
  the native context through `attach_pending_identity_to_ctx(ctx, ...)` and
  `ctx_identity(ctx, ...)`, so all per-context identity lookups use the stable key.

Date fixed: 2026-07-04

## Summary

Follow-on from `elasticsearch-restclient-builder-suite-timeout.md` (moved to
`../..` — that doc's suite-timeout/hang is FIXED). Once the suite
no longer hangs, `RestClientBuilderIntegTests` runs both its test methods
but both fail with narrow, specific assertion errors — a different, much
smaller bug than the original hang.

```text
JUnit version 4.13.2
.E.E
Time: 13.795
There were 2 failures:
1) testBuilderUsesDefaultSSLContext(org.elasticsearch.client.RestClientBuilderIntegTests)
java.lang.AssertionError:
Expected: an instance of javax.net.ssl.SSLHandshakeException
     but: <org.apache.http.ConnectionClosedException: Connection is closed> is a org.apache.http.ConnectionClosedException
2) testBuilderSetsThreadName(org.elasticsearch.client.RestClientBuilderIntegTests)
java.lang.AssertionError

FAILURES!!!
Tests run: 2,  Failures: 2
```

## Analysis (2026-07-03 update — root cause identified, fixed in 2026-07-04)

**This is NOT a client-side certificate-validation bug.** My first
investigation session (2026-07-02) guessed it was — the client rejecting
the server's untrusted self-signed cert and the rejection surfacing with
the wrong exception TYPE — and made two targeted attempts (throwing
`SSLHandshakeException` instead of `IOException` from the rustls
`process_new_packets()` error path in `t27_tls.rs::do_unwrap`; and removing
a duplicate/shadowing `SSLContext.createSSLEngine()` registration in
`phases_late.rs` that made a fully-fake, non-cryptographic `SSLEngine` stub
win over the real rustls-backed one via last-writer-wins registration
order). **Neither changed the test's observed failure at all** — same
`ConnectionClosedException`, byte-for-byte, both times. (The first fix —
correct exception typing for a genuine rustls handshake error — was kept as
a real, valid improvement even though it wasn't the culprit here. The second
was reverted: it's unproven, higher-risk — `createSSLEngine` is used
broadly, and existing test comments elsewhere in the codebase assert
`phases_late.rs` as the "canonical" owner for a different build
configuration — so it wasn't worth carrying forward for zero measured
benefit on this bug.)

**Ground truth via wire capture** (`CRATONVM_SOCKET_CAPTURE=<prefix>`, set as
an env var before invoking the suite runner — captures raw bytes at the
`net_read0`/`net_write0` layer, below TLS):

```text
w fd=1610612737 len=236 16030100e7010000e303032582664c13...   (client sends ClientHello)
r fd=1610612738 len=236 16030100e7010000e303032582664c13...   (server receives it)
w fd=1610612739 len=236 16030100e7010000e303034a24b0faac...   (2nd connection attempt: same)
r fd=1610612740 len=236 16030100e7010000e303034a24b0faac...
```

Only 4 operations total, across 2 connection attempts — a ClientHello sent
and received on each, then **nothing else. The server never sends a
ServerHello.** This is a SERVER-SIDE handshake stall, not a client-side
certificate rejection — matches the exact failure signature from
`reference_jsse_nio_sslengine_bytebuffer_hang` (a prior, different JSSE
NIO bug fixed for Tomcat) closely enough that this looks like the same
family of gap, just in a different code path.

**Root cause, high confidence**: `com.sun.net.httpserver.HttpsServer` is
real, unintercepted `sun.net.httpserver.ServerImpl`/`SSLStreams` bytecode
(confirmed: no native registration exists for `SSLStreams`/`ExchangeImpl`
in this codebase). It creates its server-side `SSLEngine` via
`SSLContext.createSSLEngine()`, which (in the current, unmodified
registration order) resolves to `net_phase_e.rs`'s real rustls-backed
engine — object class `sun/security/ssl/SSLEngineImpl`, whose
`wrap`/`unwrap`/handshake natives live in
`t27_tls.rs::register_sslengine_real`. For the SERVER branch,
`engine_begin` (`../../../../native-builtins/src/t27_tls.rs`, ~line 3147) needs this
engine's **per-`SSLContext` identity** (the keystore cert+key the test's
`getSslContext()` configured) via `state.identity_override`, which is
populated at `createSSLEngine()` time by looking up
`t27_tls::ctx_identity(sslctx)` — a lookup keyed by
`engine_objref_key(ctx_obj)` (`t27_tls.rs` ~line 2789), which hashes
`format!("{:?}", ObjectRef)`. **`ObjectRef` (`types/src/value.rs:62`) is a
`#[derive(Debug)]` wrapper around a bare `NonNull<u8>`** — its Debug output
IS the raw pointer address. `ctx_identity_table`/`ctx_trust_roots_table`
(both keyed this way) are populated once, at `SSLContext.init()` time
(`attach_pending_identity_to_ctx`, called from `net_phase_e.rs:6462`), and
read later, at `createSSLEngine()` time (could be a different thread, and
is necessarily a different point in program execution with more allocation
in between). **If a moving GC cycle relocates the `SSLContext` object
between those two points — entirely plausible given how much the real test
allocates between `getSslContext()` and the first HTTP request — the hash
key changes and the lookup silently misses.** `engine_begin` then falls
through to `default_engine_server_config`'s process-global-only identity
lookup (`runtime_tls_identity()`), which is unset in this scenario, and
fails with `"No TLS key/cert configured; set javax.net.ssl.keyStore"` —
never producing a ServerHello. I independently reproduced this EXACT error
message in a minimal standalone `SSLEngineRepro.java` (raw `SSLEngine`
client/server, no `HttpsServer`/Apache involved at all), which failed the
same way on the server side, confirming this is a server-identity-lookup
problem and not something specific to `HttpsServer`'s more complex
plumbing.

This is the SAME general class of bug as several already-fixed ones in this
codebase (see memory `reference_es_hang_02_nonblocking_connect` point 2: a
`ServerState.handlers` side table keyed similarly was unrooted/unremapped
across GC moves, fixed by wiring `gc_scan_re10_handler_roots`/
`gc_update_re10_handler_refs` into `roots.rs`/`gc.rs`). **The proper fix
follows that exact established pattern**: register
`ctx_identity_table`/`ctx_trust_roots_table`'s key objects for GC root
scanning so their keys get rehashed/updated when the GC moves them (or
switch the table to a GC-stable key, e.g. an identity-hash-based key with
collision handling like `native-collections`'s `pbkdf2_key_for`, rather
than a raw address). This issue is now addressed by the direct switch to
GC-stable identity keys in `t27_tls.rs`.

`testBuilderSetsThreadName` almost certainly fails for the same reason
(same `HttpsServer`, same server-side stall — the client's `latch.await(10,
SECONDS)` times out waiting for a response that never comes since the
server never completes the handshake) — not independently investigated,
but no reason to expect a different root cause.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start <N> -Count 1 -Parallel 1 -TimeoutSec 120 `
  -RunName es-restclient-ssl-residual-repro `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir <workdir> `
  -Exe <cratonvm.exe>
```

`<N>` = the current line number of `org.elasticsearch.client.RestClientBuilderIntegTests`
in `C:\craton\CratonVM\apps\elasticsearch\cratonvm-suite\results.jit.all.tsv`
— this drifts between suite-list refreshes (observed shifting by several
positions across runs on 2026-07-02), so look it up fresh each time rather
than trusting a previously-recorded index.

## Evidence

```text
C:\craton\CratonVM-es-restclient-fixes-20260702\apps\elasticsearch-suite-runner\.suite6\results\es-sslfix-verify-20260702\all-jit\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.out.log
```

Wire-level capture (2026-07-03, `CRATONVM_SOCKET_CAPTURE` set before invoking
the suite runner) showing the server never responds to the ClientHello:

```text
C:\craton\CratonVM-es-ssl-handshake-residual-20260703\sslcap\cap.idx
C:\craton\CratonVM-es-ssl-handshake-residual-20260703\sslcap\cap.w.1610612737
C:\craton\CratonVM-es-ssl-handshake-residual-20260703\sslcap\cap.r.1610612738
C:\craton\CratonVM-es-ssl-handshake-residual-20260703\sslcap\cap.w.1610612739
C:\craton\CratonVM-es-ssl-handshake-residual-20260703\sslcap\cap.r.1610612740
```

Independent minimal repro isolating the server-identity-lookup failure
(bypasses `HttpsServer`/Apache entirely — raw `SSLEngine` client+server):

```text
C:\Users\Victor\AppData\Local\Temp\claude\C--craton-CratonVM\f1233f70-fb06-42f7-9b53-583c236d7794\scratchpad\sslrepro\SSLEngineRepro.java
```

Relevant source locations for the fix:

```text
types/src/value.rs:62              — ObjectRef struct (raw-pointer Debug)
native-builtins/src/t27_tls.rs:228 — ctx_identity_table()
native-builtins/src/t27_tls.rs:233 — ctx_trust_roots_table()
native-builtins/src/t27_tls.rs:240 — attach_pending_identity_to_ctx() (write side)
native-builtins/src/t27_tls.rs:254 — ctx_identity() (read side)
native-builtins/src/t27_tls.rs:2789 — engine_objref_key() (the unstable hash)
native-builtins/src/t27_tls.rs:3147 — engine_begin() server branch (consumer)
native-builtins/src/net_phase_e.rs:6462 — SSLContext.init() call site
```
