# `connectWithSslBundle` got `CertificateRequired` because the CLIENT sent no certificate — `SSLContext.init` filed its KeyManagers under a recycled object's key

| | |
|---|---|
| **Status** | ✅ **FIXED** — retired from `docs/known-issues/springboot/` on 2026-08-06 |
| **Cause** | `SSLContext.init` held `this`, the `KeyManager[]` and the `TrustManager[]` as raw `ObjectRef` copies across a Java upcall (`getAcceptedIssuers()`). A moving young collection there relocates all three, so the KeyManagers and the mTLS identity were filed under a **recycled object's** side-table key while every later lookup used the real one and missed. The resulting client config has no client certificate at all |
| **Fixed by** | this branch — `NativeHandleScope` rooting in **both** `SSLContext.init` implementations, plus the three sites in the client-cert resolver that hold freshly allocated argument arrays across upcalls |
| **Severity** | medium — silent loss of the client identity on any mTLS client built from an `SSLContext`; measured 1 run in 16 on Azure Linux at load 31–72 |
| **Filed** | 2026-08-06, OPEN, "Not investigated", as a side observation while closing [`jdk-httpclient-sslbundle-tls-handshake-eintr`](jdk-httpclient-sslbundle-tls-handshake-eintr-FIXED-20260806.md) |

## The original page was looking at the wrong end of the connection

It said:

> The test's connector (`TomcatServletWebServerFactory` + `Ssl` from `test.jks`)
> sets no `clientAuth`, so it should never demand one

and sent the next reader at CratonVM's rustls **`ServerConfig`**, looking for a
shared or cached client-auth requirement leaking between the eight HTTPS
Tomcats the class stands up.

That premise is false. `AbstractClientHttpRequestFactoryBuilderTests` builds
every one of those connectors from one helper, and its first line is the
`clientAuth` setting:

```java
private Ssl ssl(String... ciphers) {
    Ssl ssl = new Ssl();
    ssl.setClientAuth(ClientAuth.NEED);      // <- apps/spring-boot/module/spring-boot-http-client/
    ssl.setKeyPassword("password");          //    src/test/java/org/springframework/boot/http/client/
    ssl.setKeyStore("classpath:test.jks");   //    AbstractClientHttpRequestFactoryBuilderTests.java:216
    ssl.setTrustStore("classpath:test.jks");
```

So the server is *supposed* to demand a client certificate, and
`CertificateRequired` is the correct alert for a client that presents none.
Nothing on the server side is wrong. The defect is that CratonVM's own
`java.net.http.HttpClient`, configured with an `SSLContext` that has
KeyManagers, sometimes presents **no certificate**.

## What the message shape already told us

The reported text carries no `TLS handshake: ` prefix:

```
java.io.IOException: HttpClient request failed: received fatal alert: CertificateRequired
```

`net_phase_e::http_exchange_rustls` prefixes every error raised inside its
handshake loop; only errors from *after* it — `write_all`, `flush`,
`http_read_response` — come out bare (`re5_do_request`'s mapper,
`net_phase_e.rs:10531`). That is the TLS 1.3 signature: the client completes
its side and sends its Finished before the server has verified anything, so a
server that rejects an empty `Certificate`
(`rustls-cbc/src/server/tls13.rs:1074`, `Error::NoCertificatesPresented`)
is only heard on the next write. A TLS 1.2 failure would have surfaced inside
the loop, as an `SSLHandshakeException`.

## The client path, and the single point where it degrades silently

`JdkClientHttpRequest.executeInternal` drives `HttpClient.sendAsync(...).get()`
(not `send`), which CratonVM answers synchronously on the calling thread with
`net_phase_e::re5_do_request`. That reads the client's `SSLContext` and calls
`t27_tls::client_config_for_ssl_context_with_ciphers`, which looks the context
up in three side tables — `ctx_key_managers_table`, `ctx_identity_table`,
`ctx_trust_roots_table` — all keyed by `ctx_obj_key`.

* KeyManagers found ⇒ the config installs `JavaKeyManagerResolver`, which
  consults the real `X509KeyManager.chooseClientAlias`/`getPrivateKey`
  mid-handshake.
* **Nothing found ⇒ `ClientAuthMode::Fixed(None)`** — an anonymous client, no
  error, no log line. rustls then sends an empty `Certificate` and the peer
  answers `CertificateRequired`.

There is no other way for this test to produce this alert.

## Root cause, measured

`SSLContext.init(km, tms, random)` does its work through three helpers. The
first one re-enters Java: `attach_trust_managers_to_ctx` calls
`getAcceptedIssuers()` on every `TrustManager`. A 2026-08-01 fix already
recognised that this window relocates objects — and pinned **only the manager
`ObjectRef`s**, which is what that page was about. The caller's own copies of
`this`, `kms_arr` and `tms_arr` were left raw, and the handler's comment even
states the hazard while relying on ordering alone to avoid it:

> native-call argument pins survive that collection, but the copied ObjectRefs
> above do not get rewritten afterwards

Ordering does not help when the *first* call is the one that allocates.

Windows, `CRATONVM_DBG_GC_STRESS=1048576` + `CRATONVM_DBG_TLS_AUTH=1`, inside
**one** `SSLContext.init` call:

```
[dbg-tls-auth] re6 SSLContext.init           key=1107
[dbg-tls-auth] attach_trust_managers_to_ctx  key=4754528796672  tms_array_present=true count=1
[dbg-tls-auth] attach_key_managers_to_ctx    key=40643275522048 kms_array_present=true count=0
[dbg-tls-auth] attach_pending_identity_to_ctx key=40643275522048 STORING km identity …
[dbg-tls-auth] ctx_identity                  key=4754528796672  trust_roots=None
```

Reading those keys (`ctx_obj_key` packs `identity_hash << 32 | generation`):

| line | identity hash | meaning |
|---|---:|---|
| `re6 SSLContext.init` | 1107 | the real `SSLContext` |
| `attach_trust_managers_to_ctx` | 1107 | still correct — runs before anything allocates |
| `attach_key_managers_to_ctx` | **9463** | a *different, recycled* object |
| `attach_pending_identity_to_ctx` | **9463** | same |
| `ctx_identity` (the next real lookup) | 1107 | **misses** — `trust_roots=None` |

Two further details make it worse than a plain misfile:

* `count=0` — the `KeyManager[]` read back through the stale reference has
  length 0, so `attach_key_managers_to_ctx` took its `list.is_empty()` branch
  and **removed** the entry rather than inserting one. Even a later correct
  lookup finds nothing.
* the same stale `this` is written into `NEW13_CTX_KM`/`_TM`/`_RANDOM` by the
  sibling handler, i.e. stale references planted in a live object's fields.

That leaves the context with no KeyManagers, no mTLS identity and no trust
roots. Which is the reported failure.

### The same run fails loudly one step earlier, which is the convenient oracle

`probes/HttpClientClientAuthProbe.java` (added by this branch) cannot even
finish its setup under GC stress on the pre-fix binary:

```
java.lang.IllegalStateException: setNeedClientAuth(true) requires javax.net.ssl.trustStore
```

That is the *server-socket* half of the identical miss:
`sss_client_ca_pem()` reads the thread-local that `ctx_identity` fills from
`ctx_trust_roots_table` — the lookup that just came back `None` above.

## Reproduced by switching the defect, because waiting does not work

Windows has no shortage of GC; it has a shortage of GCs landing inside a window
one Java upcall wide. On an idle box, **800 probe handshakes across 8 concurrent
lanes were 800/800 green** (`HttpClientClientAuthProbe`, 100 iterations × 8,
each a fresh `SSLContext` + fresh `HttpClient` against an in-process TLS server
with `setNeedClientAuth(true)`). `CRATONVM_DBG_GC_STRESS` is what puts a
collection in that window on demand.

**Positive control first.** The probe has a `nocert` mode whose client
`SSLContext` has no KeyManagers at all. On **HotSpot 25** it reproduces the
reported alert verbatim, which is what proves the probe's server genuinely
demands a certificate and that its greens are not vacuous:

```
PROBE-RESULT mode=nocert iterations=5 ok=0 failed=5
PROBE-FAILURE x4 :: javax.net.ssl.SSLHandshakeException:
                    (certificate_required) Received fatal alert: certificate_required
```

| arm | binary | `GC_STRESS` | result |
|---|---|---|---|
| positive control (`nocert`), HotSpot 25 | — | — | **5/5 fail**, `certificate_required` |
| baseline, 8 lanes × 100 | pre-fix | — | 800/800 pass |
| **defect on** | **pre-fix** | **1048576** | **`SSLContext.init` files under two different keys; `count=0`; `ctx_identity … trust_roots=None`; run dies at setup** |
| fix on | post-fix | 1048576 | see "Verification" below |

## The fix

Root the references in a `NativeHandleScope` and re-read them at every use.
The scope closes on normal return, on `?`, and on unwind.

| file | what was holding a raw `ObjectRef` across an upcall |
|---|---|
| `net_phase_e.rs` — `SSLContext.init` | `this`, `kms_arr`, `tms_arr` across `attach_trust_managers_to_ctx` |
| `phases_late/ssl_security.rs` — `SSLContext.init` (the **sibling** implementation) | the same three, plus `sr_arg`, plus the `this.as_ptr()` key for `p68_ctx_trust_roots_table` taken after `p68_extract_trust_manager_roots` re-entered Java, plus four field writes fed from the pre-upcall copies |
| `t27_tls.rs` — `materialize_java_string_array` | the array, across one `create_string` per element |
| `t27_tls.rs` — `build_issuer_principals` | the array across every element's allocation, and each `X500Principal` across its own `create_string` |
| `t27_tls.rs` — `JavaKeyManagerResolver::resolve_via_java` | `key_type_arr` and `issuers_arr`, built before the loop and passed to `chooseClientAlias` on every iteration; and the receiver, re-read *before* rather than after the `create_string` that builds `getPrivateKey`'s argument |

The resolver sites matter independently: a stale `String[] keyType` makes a real
`SunX509KeyManagerImpl.chooseClientAlias` return `null`, which this code cannot
distinguish from an application legitimately declining — the same empty
`Certificate`, from a different cause.

Finding the sibling `SSLContext.init` in `phases_late/ssl_security.rs` is the
point: fixing only the handler the trace named would have been the same partial
pass the 2026-08-01 manager-pinning fix was.

### And a tripwire, so a recurrence names itself

`JavaKeyManagerResolver::resolve` now emits a `tracing::warn!` when the context
**has** KeyManagers but resolved no certificate. That is legitimate when the
application's own `chooseClientAlias` declines, and it is also how every
internal failure in this path presents; either way the peer's answer names
neither this VM nor this decision. The warning names the decision, and points
at `CRATONVM_DBG_TLS_AUTH=1` for the per-stage trace.

## Verification

Binaries (both from this branch, so the only variable is the fix):
`cratonvm-sbtls-osr-20260806-prefix.exe` (pre-fix) and
`cratonvm-sbtls-osr-20260806.exe`.

### The A/B that carries the fix

Same probe, same lever (`CRATONVM_DBG_GC_STRESS=1048576`,
`CRATONVM_DBG_TLS_AUTH=1`), one `SSLContext.init` call each:

| | pre-fix | post-fix |
|---|---|---|
| `re6 SSLContext.init key=` | 1107 | 1107 |
| `attach_trust_managers_to_ctx` | `4754528796672` | `4754528796672` |
| `attach_key_managers_to_ctx` | **`40643275522048`** | `4754528796672` |
| … its `count=` | **0** — removes the entry | 1 |
| `attach_pending_identity_to_ctx` | **`40643275522048`** | `4754528796672` |
| `ctx_identity` (the next real lookup) | **`trust_roots=None`** | `trust_roots=Some(2)` |

Post-fix every line is `1107 << 32`, i.e. the identity the `init` line itself
names, and the second context in the same run repeats it
(`re6 SSLContext.init key=9693` → `attach_trust_managers_to_ctx
key=41631118000128` = `9693 << 32`).

### Regression

| check | result |
|---|---|
| `JdkClientHttpRequestFactoryBuilderTests` (the reported class), post-fix | **3 × `tests=32 failed=0 containersFailed=0`** |
| `HttpClientClientAuthProbe`, 8 lanes × 100, post-fix | **800/800** |
| same, pre-fix (baseline for that number) | 800/800 |
| `cargo test -p cratonvm-native-builtins --lib` | **3294 passed, 0 failed**, 6 ignored |
| `cargo test -p cratonvm-types --test doc_citation_paths` | 15 / 7 violations — **identical to an unmodified `dev` worktree**, so unchanged by this branch (both guards are red on `dev` already) |

### One thing the lever cannot do, and it is not this fix's

At `CRATONVM_DBG_GC_STRESS=1048576` the probe's *requests* wedge, repeating

```
STW cross-thread JIT takeover is still waiting for cooperative mutators
```

and never terminate — with `--nojit` too. That is `re5_do_request`'s TLS branch
deliberately not calling `begin_blocking_region` (its own comment says so),
so an HTTPS request blocks in socket syscalls while the GC still counts the
thread as a cooperative mutator; the long comment above that code documents the
same deadlock being fixed on the plain-HTTP branch in 2026-07-15. It is a
function of collection frequency, not of this change: at
`CRATONVM_DBG_GC_STRESS=33554432` **both** binaries run 10/10 clean. Filed
separately. It is why the table above reads the `init` trace rather than a
`PROBE-OK` line — a hung run and a clean short one look identical otherwise.

## Affected classes

- `module/spring-boot-http-client` —
  `org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests`
  (`connectWithSslBundle`, ~1 run in 16 under load on Azure Linux; 0 in ~10 on
  Windows, and 0 in 800 probe handshakes here — this needs a collection inside a
  window a single upcall wide).

## See also

- [`jdk-httpclient-sslbundle-tls-handshake-eintr-FIXED-20260806`](jdk-httpclient-sslbundle-tls-handshake-eintr-FIXED-20260806.md)
  — the page this was split out of. Its closing section reported this alert as
  "Unexplained, pre-existing, and not investigated here", and correctly noted
  it was present on both of its arms at the same rate.
- `tomcat-embedded-server-keystore-empty-cert-chain-intermittent-FIXED.md` —
  the same family (an identity-keyed side table that stopped finding its own
  entries after a GC move), which is why `identity_hash_code` mints lazily and
  durably in the first place.
- `internal/fixed-suite-bugs/tomcat/21-tls-handshake-enforcement-gap-FIXED.md`
  — uses this same alert as a *deliberate* signature for a genuinely rejected
  handshake, so most `CertificateRequired` grep hits are that, not this.
