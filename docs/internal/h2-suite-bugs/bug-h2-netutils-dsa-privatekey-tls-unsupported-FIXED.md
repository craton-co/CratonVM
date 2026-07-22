# H2 `TestNetUtils`: rustls/`ring` rejects H2's legacy 1024-bit RSA test key — FIXED

## Status
**FIXED** — 2026-07-22, `dev`. Originally opened 2026-07-21 as
`docs/known-issues/h2-suite-bugs/bug-h2-netutils-dsa-privatekey-tls-unsupported.md`.
Four distinct bugs were found and fixed in the same investigation, all
required to get `org.h2.test.unit.TestNetUtils` past this point cleanly (no
exceptions that fail the test, no hang).

## Original symptom
```
org.h2.jdbc.JdbcSQLNonTransientException: IO Exception:
  "java.io.IOException: ServerConfig with_single_cert failed: unexpected error:
   failed to parse private key as RSA, ECDSA, or EdDSA;
   legacy DSA TLS fallback: key is not DSA"; "port: 9111 ssl: true"
	at org/h2/util/NetUtils.createServerSocket(NetUtils.java:172)
```

## Root cause (bug 1 — the doc's headline)
The original doc's title was a red herring inherited from the error text's
own wording ("legacy DSA TLS fallback"). The actual key is **not DSA at
all**: `org.h2.security.CipherFactory.getKeyStore()` hardcodes a real,
complete (all CRT components present, 635-byte PKCS#8) **1024-bit RSA**
private key, generated back in 2005, as H2's bundled self-signed test
identity. Confirmed via `CRATONVM_DBG_TLS_HS=1` tracing: the DER CratonVM's
TLS layer receives is byte-identical to the hex literal in
`CipherFactory.java`, and is structurally a well-formed RSA `PrivateKeyInfo`.

`rustls::sign::any_supported_type` (via CratonVM's `ring` crypto backend,
`native-builtins/src/t27_tls.rs`) tries RSA, then ECDSA, then EdDSA, and
returns one generic error if all three fail — `ring::rsa::KeyPair::from_pkcs8`
enforces a **hard-coded 2047-bit minimum modulus** (confirmed in `ring`
0.17.14's own source, `src/rsa/keypair.rs`: "the public modulus (n) must be
at least 2047 bits"), so a syntactically-valid-but-1024-bit RSA key is
rejected — not a parsing bug, a key-size policy floor. The existing "legacy
DSA" OpenSSL fallback (`legacy_dsa_acceptor`) then also declined it, since
it gated strictly on `key.dsa().is_ok()` and this key genuinely isn't DSA.

### Fix
`native-builtins/src/t27_tls.rs`: `legacy_dsa_acceptor` no longer gates on
key type. It was already an OpenSSL-backed fallback specifically for
identities `ring`'s stricter policy refuses (originally written for DSA,
which rustls has zero support for at all) — generalized to accept *any* key
OpenSSL itself can use (`set_security_level(0)` already makes OpenSSL
permissive about weak/legacy material). Enum/variant names (`LegacyDsa`)
were left as-is — used identically in `servlet.rs`'s unrelated client-side
DSA-cert-trust bridge — to keep the diff scoped to the actual behavior
change.

## Bug 2 — `SSLServerSocket.getEnabledProtocols`/`setEnabledProtocols` never registered
Once bug 1 let `createServerSocket` itself succeed, `CipherFactory
.createServerSocket` immediately calls
`secureSocket.setEnabledProtocols(disableSSL(secureSocket.getEnabledProtocols()))`.
Neither method had ever been registered on `javax/net/ssl/SSLServerSocket`
(only `SSLSocket`/`SSLEngine` had them, via a different, engine-backed
implementation) — `AbstractMethodError: method
javax/net/ssl/SSLServerSocket.getEnabledProtocols()... has no Code
attribute`.

### Fix
`native-builtins/src/t27_tls.rs`: registered `getEnabledProtocols`/
`setEnabledProtocols`/`getSupportedProtocols` on `SSLServerSocket`, backed by
a small `gc_stable_objref_key`-indexed side table (same pattern as the
existing `sock_alpn_table`), since the class's compact synthetic layout has
no spare field for a `String[]`.

## Bug 3 — `SSLSocketFactory.createSocket()` (zero-arg) returned a plain `Socket`
`NetUtils.createLoopbackSocket` → `CipherFactory.createSocket` uses the
standard JSSE "unconnected socket, connect later" idiom:
`SSLSocket s = (SSLSocket) f.createSocket(); s.connect(addr, timeout);`.
CratonVM had no registration for the true zero-arg `createSocket()` overload
on `javax/net/ssl/SSLSocketFactory` itself, so the call fell through (via
ancestor-class native lookup) to `javax/net/SocketFactory`'s own generic
zero-arg native, which allocates a **plain `java/net/Socket`** —
`ClassCastException: java.net.Socket cannot be cast to javax.net.ssl.SSLSocket`
on every one of the ten `ConnectWorker` threads.

### Fix
`native-builtins/src/phases_late.rs`:
- Registered `SSLSocketFactory.createSocket()` to allocate a proper
  `javax/net/ssl/SSLSocket`-classed object in a "pending" state
  (`NEW13_SOCK_TLSID = -1`), stashing the factory's trust roots/TrustManager
  key (if any) in a new identity-keyed side table
  (`pending_ssl_socket_connect_ctx_table`) for `connect()` to pick up later.
- Registered `SSLSocket.connect(SocketAddress[, int])` (real
  `javax.net.ssl.SSLSocket` doesn't redeclare `connect` — it's inherited,
  concrete, from `java.net.Socket` — but a class-specific registration here
  still intercepts first, ahead of the ancestor's plain-TCP-only native) to
  perform the real TCP connect + TLS client handshake and populate the
  *existing* socket object.
- Refactored the shared connect+handshake logic out of
  `new13_do_create_socket` into `new13_connect_and_handshake` (returns just
  the stream id) and the field-population tail into `new13_finish_socket`
  (now takes the socket object as a parameter instead of always allocating
  one), so both the immediate-connect and deferred-connect paths share one
  implementation.

## Bug 4 — `SSLServerSocket.close()` deadlocked against a blocked `accept()`
With bugs 1–3 fixed, `TestNetUtils.testFrequentConnections` **hung**
(timed out, never completed) instead of throwing. `NetUtils.createServerSocket`'s
background `Task` thread loops `serverSocket.accept()`; H2's own client
workers can all exhaust their loop and exit without ever completing a real
connection (as they did here, after bugs unrelated to this one), leaving that
thread parked in a real, unbounded, blocking `TcpListener::accept()` call.

The bug: `rustls_server_accept` held the shared `sreg()` registry mutex for
the *entire duration* of that blocking OS call. The test's own `finally`
block calls `serverSocket.close()` from the main thread — `rustls_listener_close`
needs that same mutex to remove the listener entry — permanently deadlocked
against a thread that can never release a lock it's blocked inside the
kernel while holding.

### Fix
`native-builtins/src/t27_tls.rs`: `rustls_server_accept` now holds `sreg()`
only briefly, to `try_clone()` the `TcpListener` handle and clone a new
`Arc<AtomicBool>` "closed" flag (added to `TlsServerListenerEntry`), then
polls the cloned listener non-blockingly (20ms interval) outside the lock,
checking the closed flag between attempts. `rustls_listener_close` sets that
flag before/when removing the entry, so a concurrent `close()` always
acquires the mutex immediately and any parked accept-poll notices within one
interval.

## Verification
- `org.h2.test.unit.TestNetUtils` (exact doc repro, real class, full run):
  **PASS** — exit code 0, no `ClassCastException`/`AbstractMethodError`/DSA
  parse error, no hang. Re-run 3× consecutively for stability.
- `cargo test -p cratonvm-native-builtins --lib`: 3057 passed, 0 failed, 6
  ignored (baseline unchanged).
- Standalone `SslEchoProbe.java` (self-contained, keytool-generated 2048-bit
  RSA cert): confirmed the ordinary (non-legacy-key) `SSLServerSocketFactory
  .createServerSocket`/`SSLSocketFactory.createSocket()+connect()` round trip
  still completes a real handshake and exchanges data end-to-end after the
  `rustls_server_accept` locking change — the shared accept hot path used by
  every other TLS-serving suite is not regressed by bug 4's fix.
- Built with `CARGO_PROFILE_RELEASE_LTO=off` on the Azure Linux host in an
  isolated worktree/branch (`fix/h2-netutils-dsa-privatekey-tls-20260721`),
  not the shared main checkout.

## Known, separate, NOT-fixed-here finding
While running `SslEchoProbe.java` above, reading/writing on the `Socket`
returned by `SSLServerSocket.accept()` produced no data (`read()` returned
-1 immediately) — the accepted socket's TLS stream id write
(`SSS_SOCK_TLSID`) appears to be silently dropped the same way
`new13_do_create_socket`'s equivalent write once was (see that fix's own
doc comment in `phases_late.rs`), but a first attempt to apply the same
`sock_set_for_create` side-table fix here produced a *different* failure
(`no such TLS stream id`) plus an outright hang — the accept-path stream id
and the client-path stream id apparently live in two registries
(`t27_tls::sreg()` vs. `servlet::s2_registry`) that the shared I/O natives
don't cleanly reconcile. Reverted rather than ship a half-understood change
to this shared, heavily-used accept path. **Not required for this doc**: H2's
own `TestNetUtils` never reads or writes on an accepted socket (only
`accept()` + `close()`), so this doesn't block the fix above. Flagged as a
follow-up for whoever next touches `SSLServerSocket.accept()` blocking I/O.

## Files changed
- `native-builtins/src/t27_tls.rs` — bug 1 (`legacy_dsa_acceptor` gate
  removed), bug 2 (`SSLServerSocket` protocol natives + side table), bug 4
  (`TlsServerListenerEntry.closed` flag, non-blocking `rustls_server_accept`
  poll loop, `rustls_listener_close` flag signal).
- `native-builtins/src/phases_late.rs` — bug 3 (`SSLSocketFactory
  .createSocket()` zero-arg registration, `SSLSocket.connect` registrations,
  `new13_connect_and_handshake`/`new13_finish_socket` refactor,
  `pending_ssl_socket_connect_ctx_table`).
