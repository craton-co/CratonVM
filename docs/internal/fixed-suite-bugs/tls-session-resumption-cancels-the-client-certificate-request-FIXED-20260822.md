# A resumed TLS session cancelled the client-certificate request — `TestCustomSslTrustManager` and most of `TestClientCert`

| | |
|---|---|
| **Status** | ✅ FIXED — 2026-08-22, `native-builtins/src/t27_tls.rs` |
| **Severity** | high — every mutual-TLS exchange Tomcat obtains lazily |
| **HotSpot** | PASS (measured, this fixture) |
| **CratonVM** | `TestCustomSslTrustManager` 2 failures → **OK (9 tests)**; `TestClientCert` **5 failures → 1** (the remaining one is not this defect — see Residual) |
| **Root cause** | the server session cache was shared across client-auth policies, so a connection that DID request a client certificate resumed one that had not — and an abbreviated handshake carries no `CertificateRequest` |

## The measurement that named it

The tempting reading of

```
javax.net.ssl.SSLHandshakeException: connection closed immediately after the TLS
handshake with no response — the peer likely rejected the handshake (e.g. a
required client certificate was not presented)
```

is "our client has no certificate to send". It is the opposite. With
`CRATONVM_DBG_TLS_AUTH=1` on `dev@151f7831a`:

```
capture_huc_key_managers_ctx_key key=8632884264960 has_kms=true
build_engine_client_config_with_identity km_ctx_key=Some(...) client_identity_present=true
JavaKeyManagerResolver::has_certs CALLED ... -> true          x16
JavaKeyManagerResolver::resolve  CALLED                       x0
```

**`has_certs` 16 times, `resolve` zero times.** The resolver was fully
configured and never consulted — so the peer never asked. That moves the whole
question to the server.

## Why the server did not ask

rustls implements **no TLS 1.2 renegotiation** (deliberate; its own manual cites
CVE-2009-3555 and 3SHAKE). Tomcat's lazy client auth needs one: the connector
accepts a connection with no client auth, and only when a request reaches a
protected resource does `SSLAuthenticator` call `setNeedClientAuth(true)` and
rehandshake.

CratonVM already emulates that — `wants_deferred_client_auth`. `engine_begin`
recognises the post-handshake switch, fails that connection with *"TLS
renegotiation is not supported; the client certificate will be requested on the
next handshake for this connector"*, marks the `SSLContext`, and the request is
re-sent on a fresh connection where client auth IS offered up front. The trace
confirms the mechanism fired:

```
engine_begin SHORT-CIRCUIT (conn already realized) need=true want=false   <- engine 1
engine_begin request=true client_ca_none=false use_passthrough_verifier=true  <- engine 2
```

Engine 2 offered client auth. And then:

```
server store: get len=32 -> true
do_unwrap id=2 process_new_packets -> Ok is_handshaking=true
do_unwrap id=2 process_new_packets -> Ok is_handshaking=false
```

Two unwrap rounds — an **abbreviated** handshake. The client resumed engine 1's
session, and a resumed TLS 1.2 session replays the original handshake's outcome:
no `CertificateRequest` is sent, so the client is never asked. The resumption
silently cancelled the request the retry existed to make.

## Fix

`ctx_server_session_store` is now keyed on `(ssl_context_key,
client_auth_requested)` rather than on the context alone, and `engine_begin`
passes `state.client_auth_requested` — which the line immediately above already
maintains for its own rehandshake detector.

Sessions established WITH client auth still resume among themselves, so the
cost is one full handshake per policy transition, not per connection.

Real JSSE reaches the same outcome by a different route: its session object
carries the peer certificates and it declines to resume into a connection whose
client-auth requirement that session cannot satisfy. rustls's
`StoresServerSessions` is an opaque blob store with no such visibility, which is
why the partition lives in the key rather than in a predicate. No ticketer is
configured, so TLS 1.3 stateful tickets go through the same store and are
covered by the same partition.

**Regression test:**
`t27_tls::tests::a_server_session_cache_is_partitioned_by_client_auth_policy`
asserts `Arc::ptr_eq` both ways — two distinct caches across the policy
boundary, one shared cache within it. The second half matters: a partition that
never shares would have disabled server-side resumption altogether and still
passed a difference-only check. Negative control (key reverted to `(key,
false)`, test kept): fails on the first assertion.

## Verification

`bin/cratonvm-tls-7b7f66ee5`, Azure Linux, real JDK 25, `apps/tomcat` fixture:

| Class | before | after |
|---|---|---|
| `TestCustomSslTrustManager` | 2 failures / 9 | **OK (9 tests)** |
| `TestClientCert` | 5 failures / 18 | 1 failure / 18 |

Full 16-class SSL/TLS + OCSP sweep on the same binary: **14 green**, up from 12.
No class that was green before this change is red after it. The two that are not
green are the renegotiation-emulation limit and the OCSP stop-the-world stall,
both tracked in `known-issues/tomcat/`; a one-off `TestSsl.testPost` red in that
sweep re-ran clean 2 of 3 times on the same binary and is recorded there as a
load-sensitive flake.

`cratonvm-native-builtins` 4129 passed / 1 failed — the same
`proxy_selector::tests::env_proxy_lookup_respects_case_insensitive_windows_storage`
that fails on the merge base with these changes reverted.

## Residual — one test, and it is not this defect

`TestClientCert.testClientCertPostZero[JSSE]`: `expected:<OK-[0]> but
was:<OK-[1024]>`. The client certificate now works (the response carries the
role); what differs is the request body.

That test sets `maxSavePostSize(0)`, which tells Tomcat **not** to buffer the
POST body across the client-auth renegotiation — so on a real JSSE run the body
is discarded and the servlet sees 0 bytes. CratonVM does not renegotiate; it
re-sends the request on a new connection, so the server reads the full 1024-byte
body and answers `OK-1024`. The assertion is on Tomcat's internal buffering
limit, which only has meaning when the certificate is obtained *without* a new
request.

This is inherent to the retry-on-next-connection emulation, not a bug in it:
matching would require the VM to know and honour a Tomcat connector setting, and
dropping a body the client legitimately re-sent would be wrong everywhere else.
`wants_deferred_client_auth`'s doc already named the deviation ("one extra TCP
connection, and the request is re-sent on it"); this is the one assertion in the
suite that can see it. Tracked with the other renegotiation-emulation limits in
`known-issues/tomcat/`.
