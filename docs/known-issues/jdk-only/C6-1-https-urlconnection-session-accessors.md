# C6-1 — the `https:` carrier was the ABSTRACT class, and the fix is two halves

**2026-08-12, lane C6.** Carried over as a one-line nomination from
`P4A-TOMCAT-20260812.md` §3 (lane B3). This record **verifies** that
nomination, finds it **incomplete**, and lands the complete form.

**This lane could not build or run the VM.** Every HotSpot number below was
executed on this host against HotSpot 25.0.3+9-LTS
(`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`). Every CratonVM "after"
value is marked **PREDICTED**. The registry facts are read from an authoritative
`--dump-native-registry` dump, not from source order.

**Not a TLS defect.** CratonVM already serves HTTPS with a certificate-verified
handshake (`P4A-TOMCAT-20260812.md` §2). The gap is the `HttpsURLConnection`
*client adaptor* over a stack that works.

---

## 1. The nomination was right about the class and wrong about the size

`URL.openConnection()` on an `https:` URL returned
`javax/net/ssl/HttpsURLConnection` **itself**, which is `abstract`. Verified on
this host:

```
$ javap -p --module java.base javax.net.ssl.HttpsURLConnection
public abstract class javax.net.ssl.HttpsURLConnection extends java.net.HttpURLConnection {
  public abstract java.lang.String getCipherSuite();
  public abstract java.security.cert.Certificate[] getLocalCertificates();
  public abstract java.security.cert.Certificate[] getServerCertificates() throws ...;
  public java.security.Principal getPeerPrincipal() throws ...;
  public java.util.Optional<javax.net.ssl.SSLSession> getSSLSession();
```

So this is the **abstract-declaration** shape, not a fabricated receiver — the
class is real. B3's diagnosis holds.

**What B3's nomination missed is what the concrete subclass actually contains.**
Every method on it is a delegate hop:

```
$ javap -p -c --module java.base sun.net.www.protocol.https.HttpsURLConnectionImpl
  private final sun.net.www.protocol.https.DelegateHttpsURLConnection delegate;

  public java.lang.String getCipherSuite();
    Code:
       0: aload_0
       1: getfield      #50   // Field delegate:Lsun/net/www/protocol/https/DelegateHttpsURLConnection;
       4: invokevirtual #77   // Method ...DelegateHttpsURLConnection.getCipherSuite:()Ljava/lang/String;
       7: areturn
```

`delegate` is assigned only in `HttpsURLConnectionImpl(URL, Proxy, Handler)`.
CratonVM's carrier is **allocated, never constructed** —
`try_alloc_concurrent_synthetic` then a handful of `set_field`s — so `delegate`
is null.

**Therefore B3's one-line swap, landed alone, only exchanges one exception for
another:** `AbstractMethodError` becomes `NullPointerException` on
`getfield delegate`, on all five certificate/cipher accessors **and on
`getSSLSession()`**, which today does not throw at all. A caller doing
`conn.getSSLSession().ifPresent(...)` currently no-ops and would start crashing.
That is this project's own recorded failure mode — *a fix that moves failures
deeper can worsen the count*.

## 2. The registry says the accessors exist nowhere

From `scratchpad/p1/reg.json` (`--dump-native-registry`, 11,748 natives):

| query | answer |
|---|---|
| `java/net/URL.openConnection()` owner | `net_phase_e.rs:8542`, `owns_slot: true`, never overwritten |
| `sun/net/www/protocol/https/HttpsURLConnectionImpl` registrations | **30**, all `owns_slot: true` |
| `getCipherSuite` / `getServerCertificates` / `getLocalCertificates` / `getPeerPrincipal` / `getLocalPrincipal` / `getSSLSession` on **either** carrier class | **zero** |

So the request surface on the Impl is already fully wired (the swap is safe),
and the six session accessors are simply absent. `getServerCertificates` is
registered **nowhere in the workspace**, on any class.

The dump also settles an ordering question the source cannot: for the shared
request surface (`connect`, `getResponseCode`, `getInputStream`, …)
`http_url_connection.rs`'s `register_one` **overwrites** `net_phase_e.rs`'s
copies — this file's rows read `owns_slot: false`, `overwrote: bridge` on the
winner. None of the six names above appear in `register_one`, so the new
registrations cannot be overwritten by it. **If a later change adds one there,
that copy wins and this one goes silently dead. Check the dump, not the source
order.**

## 3. The unconnected shape, measured rather than assumed

Writing the "no session yet" arm required knowing what HotSpot does. It is not
`Optional.empty()`, and it is not six different things — `scratchpad/c6/NotOpen.java`:

```
class = sun.net.www.protocol.https.HttpsURLConnectionImpl
getCipherSuite        THREW java.lang.IllegalStateException: connection not yet open
getServerCertificates THREW java.lang.IllegalStateException: connection not yet open
getLocalCertificates  THREW java.lang.IllegalStateException: connection not yet open
getPeerPrincipal      THREW java.lang.IllegalStateException: connection not yet open
getLocalPrincipal     THREW java.lang.IllegalStateException: connection not yet open
getSSLSession         THREW java.lang.IllegalStateException: connection not yet open
```

`getSSLSession()` included. **So CratonVM's silent `Optional.empty()` is wrong
even for a connection that never handshook**, and answering
`IllegalStateException` can never regress a caller relative to the oracle. This
is what makes the two halves separable in the *safe* direction: with the class
swapped and the accessors registered but their populator not yet landed, every
answer is HotSpot's own refusal — a missing ANSWER, never a wrong one.

## 4. What landed, in `net_phase_e.rs` (this lane's file)

1. **Carrier → `sun/net/www/protocol/https/HttpsURLConnectionImpl`.** The Spring
   `SkipSslVerificationHttpRequestFactory` `instanceof HttpsURLConnection` test
   that motivated the current value is preserved — `javap` confirms the Impl
   extends the base.
2. **`register_https_session_accessors`** — the six accessors, registered on
   **both** the Impl and the abstract base, so neither receiver can reach
   bytecode that dereferences `delegate`.
   * with a recorded session: `getCipherSuite` answers directly; the other four
     **delegate to the already-registered `javax/net/ssl/SSLSession` natives**
     (`getPeerCertificates` t27_tls:11420, `getLocalCertificates`
     ssl_security:4519, `getPeerPrincipal` ssl_security:4547,
     `getLocalPrincipal` ssl_security:4486) rather than decoding X.509 a second
     time; `getSSLSession` wraps it in a one-slot `java.util.Optional`.
   * without one: `IllegalStateException: connection not yet open`, per §3.
3. **`record_https_carrier_session`** — an identity-keyed side table holding
   **plain data only** (two `String`s and the peer chain's DER bytes), never an
   `ObjectRef`, keyed by `NativeObjKey` (VM identity + `identityHashCode`). No
   collector path needs to know about it. Same rule and same reason as
   `sock_side_table` in the same file.

It carries `#[allow(dead_code)]` **scoped to that one function**, because its
sole call site is NOMINATION 1 below. Removing the attribute is the reviewer's
cue that the call site arrived.

---

## NOMINATION 1 — the populator (one line, `http_url_connection.rs`)

> **LANDED 2026-08-12 by lane C12**, as `STEP 0`, above STEP 1, with a source
> witness pinning it there. Lane C12 also found that the `cipher` string this
> block records was rustls's spelling (`TLS13_AES_256_GCM_SHA384`) and not
> JSSE's (`TLS_AES_256_GCM_SHA384`) — i.e. the headline accessor would have
> answered a name HotSpot never produces — and fixed it at the point of
> production. See
> `C12-2-https-session-capture-and-the-cipher-name-it-records.md`; the
> `#[allow(dead_code)]` removal in `net_phase_e.rs` is nominated there.

`huc_verify_hostname` already has the connection, the protocol, the cipher and
the peer chain in scope at the same instant, and already builds an
`SSLSession` from exactly those. It just discards them.

**File:** `native-builtins/src/http_url_connection.rs`, in `huc_verify_hostname`.

REPLACE:

```rust
    // STEP 1 — the built-in check, always first and always on its own.
    let builtin = huc_builtin_endpoint_identification(host, &peer_chain_der);
```

WITH:

```rust
    // Record the negotiated session against the carrier BEFORE any early
    // return: `HttpsURLConnection.getCipherSuite()/getServerCertificates()/
    // getPeerPrincipal()/getSSLSession()` answer from this and from nothing
    // else, and STEP 1 below returns early on the common (successful) path.
    // Recorded even when identification later fails, exactly as the real JDK
    // does — the session exists once the handshake completes; whether the peer
    // is ACCEPTED is a separate question the caller's exception answers.
    if let Some(conn) = connection {
        crate::net_phase_e::record_https_carrier_session(
            ctx,
            conn,
            protocol,
            cipher,
            &peer_chain_der,
        );
    }
    // STEP 1 — the built-in check, always first and always on its own.
    let builtin = huc_builtin_endpoint_identification(host, &peer_chain_der);
```

**Ordering is load-bearing and is the reason this is not "anywhere in the
function": `huc_verify_hostname` returns early at `if builtin.is_ok()`, which is
the path every ordinary request takes.** A record placed after that point would
capture a session only for connections whose built-in name check FAILED — i.e.
it would read green on the failure path and be dead on the success path.

## NOMINATION 2 — `java.util.Optional` has two incompatible synthetic layouts

> **VERIFIED 2026-08-12 by lane C12, and it is worse than this section says.**
> The line numbers are right, but the arity disagreement is a symptom: the
> defect is that slot 0 holds an `int` presence FLAG, and on a real
> `java.util.Optional` slot 0 **is** `value` — the reference `isPresent()`
> tests for null and `get()` returns. `previousResponse()` (`http2.rs:2108`)
> uses the correct 1-slot arity and still writes an `Int`, which is how we know
> it is not about the layout. `Value::Int(0)` is not null to
> `ref_operand_is_null`, so the empty case reads as PRESENT. Nine sites, a
> measurement plan and the fix shape:
> `C12-3-optional-value-slot-holds-an-int-flag.md`.

Not this lane's defect and not measured; recorded because it was found while
choosing a layout, and a wrong guess here is silent.

* `classfile_api.rs:39` allocates `java/util/Optional` with **1** field and puts
  the value at slot 0. This matches the real class (`private final T value`).
* `http2.rs:1289` allocates it with **2** fields, slot 0 an `Int` flag and slot 1
  the value.

Both cannot be right against a real `java.util.Optional`, and the second writes
an `Int` into the slot that real bytecode reads as `value`. The new
`getSSLSession` uses the one-slot form. **Someone should adjudicate `http2.rs`
against a real-JDK `Optional.isPresent()` before it is trusted** — the shape is
only exercised by `HttpClient` config getters, which is why it has survived.

---

## How to verify — flags first, then the two probes

**FLAG ORDER FAILS SILENTLY.** `--dump-native-registry` and the `--jdk-only`
report flags placed *after* the main class name are ignored: no file, no
warning, exit 0. Put them before `-cp`.

```
cratonvm --jdk-only --dump-native-registry reg-after.json -cp . <Main>
```

Then, on `reg-after.json`, all six names must appear on **both**
`sun/net/www/protocol/https/HttpsURLConnectionImpl` and
`javax/net/ssl/HttpsURLConnection` with `owns_slot: true`. If any reads
`owns_slot: false`, a later registration took the slot and this record's fix is
inert regardless of what the source says.

Behaviourally, against the same embedded-Tomcat HTTPS fixture B3 used:

| | before | HotSpot 25 | after (PREDICTED) |
|---|---|---|---|
| `conn.getClass()` | `javax.net.ssl.HttpsURLConnection` | `sun.net.www.protocol.https.HttpsURLConnectionImpl` | `sun.net.www.protocol.https.HttpsURLConnectionImpl` |
| `getCipherSuite()` | `AbstractMethodError` | `TLS_AES_256_GCM_SHA384` | the negotiated suite |
| `getServerCertificates()` | `AbstractMethodError` | 1 certificate | the peer chain, leaf first |
| `getPeerPrincipal()` | `AbstractMethodError` | the leaf's subject | the leaf's subject |
| `getSSLSession()` | `Optional.empty()`, **silently** | present | present |
| all six, before `connect()` | — | `IllegalStateException: connection not yet open` | same |

**And re-run the Spring `SkipSslVerificationHttpRequestFactory` path**, which is
what motivated the original abstract-base value. The `instanceof` still holds by
`javap`, but that is an argument, not a measurement.

## Residuals

1. **`getLocalCertificates()` / `getLocalPrincipal()` are delegated but
   unexercised.** No client-auth request has been run through this path; they
   answer whatever `ssl_security.rs`'s `SSLSession` accessors answer for a
   session with `TLSID = -1`. Untested in both directions.
2. **Session reuse across a pooled connection is not modelled.** The table is
   keyed by the carrier object; a second request on a new carrier over a reused
   TLS session records a fresh entry, which is correct, but nothing evicts an
   entry when a carrier is collected. It is bounded by connections made, not by
   time.
3. **`P4A-TOMCAT-20260812.md` §8a(ii) is still open and is the reason this
   matters.** `TestCustomSslTrustManager.testCustomTrustManagerNone` — CratonVM
   PASSES a trust test HotSpot FAILS, and the HotSpot stack runs through
   `sun.net.www.protocol.https.HttpsURLConnectionImpl.connect`, the path
   CratonVM did not take. This change puts CratonVM on the same class. **It does
   not follow that the divergence is fixed**; that row needs re-measuring after
   this lands, and a pass is still not evidence until someone establishes what
   the `None` case is supposed to enforce.
