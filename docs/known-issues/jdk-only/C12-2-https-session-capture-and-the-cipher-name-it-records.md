# C12-2 — the `https:` session CAPTURE landed, and the name it was about to record was not JSSE's

**2026-08-12, lane C12.** Closes NOMINATION 1 of
`C6-1-https-urlconnection-session-accessors.md`: the carrier swap and the six
session accessors landed in `net_phase_e.rs`, backed by an identity-keyed side
table, with **no producer**. Every accessor therefore answered
`IllegalStateException: connection not yet open` on every connection, handshaken
or not. The producer is one block, and it is in this lane's file.

While writing it, the value the block was about to record turned out to be
spelled in rustls's dialect rather than JSSE's — measured, not suspected. That
is fixed here too, because `getCipherSuite()` is the headline accessor of the
six and landing a capture whose main value is wrong would have looked like
success.

**This lane could not build or run the VM.** HotSpot numbers are from this host
(HotSpot 25.0.3+9-LTS, `scratchpad/c12/C12Probe.java`); every CratonVM "after"
is **PREDICTED**.

---

## 1. The placement, verified before it was written

C6-1 said the capture must go **before `STEP 1`** and explained why. Verified in
the source rather than taken on trust — `huc_verify_hostname` reads:

```rust
    // STEP 1 — the built-in check, always first and always on its own.
    let builtin = huc_builtin_endpoint_identification(host, &peer_chain_der);
    ...
    if builtin.is_ok() {
        return Ok(());          // <- EVERY successful request leaves here
    }

    // STEP 2 — the built-in check failed. Consult the installed verifier ...
```

Endpoint identification passing is the ordinary outcome, so **STEP 2 onwards is
the failure path**. A capture placed anywhere below that `return` would record a
session only for connections whose hostname check failed — green under any probe
that deliberately breaks verification, and dead in production. This is the shape
this project has already paid for twice (*"a fix that only pins the positive
half"*, *"a no-op native whose comment explains why it is a no-op"*): it would
have looked implemented.

**Landed as `STEP 0`**, above STEP 1, with the reasoning at the site.

The call is inside `if let Some(conn) = connection`. `connection` is
`Option<ObjectRef>` and is kept current across the handshake's blocking regions
by the caller's `end_blocking_region_refs` remap (`http_url_connection.rs`, the
`blocked_refs` array), so the ref recorded is the live carrier and not a stale
address. Nothing allocates between that remap and STEP 0.

**Recorded even when identification later fails**, deliberately: the session
exists once the handshake completes; whether the peer is ACCEPTED is a separate
question that this function's `Err` and the caller's exception answer. A caller
that catches `SSLPeerUnverifiedException` and then asks what was negotiated gets
the same answer HotSpot gives.

### The witness

Nothing behavioural can see this ordering without a live TLS peer, so it is
asserted against the source:
`http_url_connection_tests::the_session_capture_precedes_the_success_path_early_return`
reads the **working tree** (not an `include_str!` snapshot — this repository is
edited from both Windows and Linux, and it normalises `\r\n`), isolates
`huc_verify_hostname`'s body, and fails if the
`record_https_carrier_session` call is not above `if builtin.is_ok() {`. It also
fails if the call disappears entirely, which is the other way this regresses.

## 2. The cipher name was rustls's, not JSSE's

`getCipherSuite()` on `SSLSession` and on `HttpsURLConnection` is contracted to
return the **IANA registry name**, which is what JSSE reports. rustls's
`CipherSuite` enum spells its TLS 1.3 variants differently, and
`format!("{:?}", cs.suite())` — the idiom used at this call site and at seven
more — prints the variant name:

```
$ grep TLS13_AES_256_GCM_SHA384 ~/.cargo/registry/src/*/rustls-0.23.42/src/enums.rs
        TLS13_AES_256_GCM_SHA384 => 0x1302,
```

Measured against the oracle rather than argued (`C12Probe.java` §B, HotSpot 25):

```
JSSE supports TLS_AES_256_GCM_SHA384 = true
JSSE has any TLS13_* name            = false
```

| suite | rustls `{:?}` | JSSE / HotSpot |
|---|---|---|
| TLS 1.3 AES-256-GCM | `TLS13_AES_256_GCM_SHA384` | `TLS_AES_256_GCM_SHA384` |
| TLS 1.3 AES-128-GCM | `TLS13_AES_128_GCM_SHA256` | `TLS_AES_128_GCM_SHA256` |
| TLS 1.2 ECDHE-RSA | `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256` | identical |

**Only the TLS 1.3 suites diverge, and TLS 1.3 is this client's default**, so
the divergence would have been on essentially every connection. It also failed
to close the VM's own round trip: `t27_tls::java_cipher_name_to_suite` maps
`"TLS_AES_256_GCM_SHA384"` back to rustls and has never accepted the `TLS13_`
spelling.

Fixed in this lane's file with `jsse_cipher_suite_name`, a prefix rewrite
covering exactly the five `TLS13_*` variants, applied where the string is
produced — so both the session table AND the `SSLSession` handed to an
application's `HostnameVerifier` now carry the JSSE name. Unit-tested in both
directions (a TLS 1.2 name must pass through untouched, and it is not a blanket
"anything starting with TLS" rewrite).

`protocol` needed no change: `"TLSv1.3"`/`"TLSv1.2"` is already JSSE's spelling
of `SSLSession.getProtocol()`.

## 3. The unconnected contract, re-measured

C6-1 §3 measured it; it is load-bearing for the "missing ANSWER, never a wrong
one" claim, so it was re-run on this host rather than quoted
(`C12Probe.java` §C):

```
class = sun.net.www.protocol.https.HttpsURLConnectionImpl
getCipherSuite         THREW java.lang.IllegalStateException: connection not yet open
getServerCertificates  THREW java.lang.IllegalStateException: connection not yet open
getLocalCertificates   THREW java.lang.IllegalStateException: connection not yet open
getPeerPrincipal       THREW java.lang.IllegalStateException: connection not yet open
getLocalPrincipal      THREW java.lang.IllegalStateException: connection not yet open
getSSLSession          THREW java.lang.IllegalStateException: connection not yet open
```

Confirmed, `getSSLSession()` included. Two things follow. First, the accessors'
no-session arm is right. Second — and this is the part that is easy to miss —
**`Optional.empty()` was wrong even for a connection that never handshook**, so
there is no state in which the old answer was correct.

## NOMINATION 1 — `net_phase_e.rs` (not this lane's file): the call site arrived

`record_https_carrier_session` carries `#[allow(dead_code)]` and a doc paragraph
stating it is uncalled. Both are now false. The attribute's own comment names
its removal as the reviewer's cue.

**File:** `native-builtins/src/net_phase_e.rs`.

REPLACE:

```rust
/// UNCALLED AS OF THIS COMMIT, deliberately and temporarily. The one call site
/// is a single line inside `http_url_connection.rs`'s `huc_verify_hostname`,
/// which is the only place holding all three values at once; that file belongs
/// to another lane, so it is a NOMINATION rather than an edit here. Until it
/// lands, all six accessors answer `IllegalStateException: connection not yet
/// open` -- HotSpot's own answer for an unhandshaken connection, so the
/// half-landed state is safe but useless. `#[allow(dead_code)]` is scoped to
/// this one function so its removal is the reviewer's cue that the call site
/// arrived.
#[allow(dead_code)]
pub(crate) fn record_https_carrier_session(
```

WITH:

```rust
/// The one call site is `http_url_connection.rs`'s `huc_verify_hostname`, as
/// its STEP 0 — landed 2026-08-12 by lane C12, which is why the
/// `#[allow(dead_code)]` that used to sit here is gone. **Its position in that
/// function is load-bearing and is guarded by a source witness there**: STEP 1
/// returns early on `builtin.is_ok()`, the path every successful request takes,
/// so a capture below it would record sessions only for connections whose
/// hostname check FAILED. See
/// `docs/known-issues/jdk-only/C12-2-https-session-capture-and-the-cipher-name-it-records.md`.
pub(crate) fn record_https_carrier_session(
```

## NOMINATION 2 — `t27_tls.rs` (not this lane's file): seven more rustls spellings

The same `format!("{:?}", cs.suite())` produces the name reported by
`SSLSession.getCipherSuite()` on the SSLSocket, SSLEngine and server paths, at
**seven** sites: `t27_tls.rs:3254`, `:3418`, `:3601`, `:3676`, `:5898`, `:9011`,
`:9419`. Every one of them hands an application a `TLS13_*` name HotSpot never
produces.

The narrow fix is to make this lane's helper shared and call it at each site.

**File:** `native-builtins/src/http_url_connection.rs` (this lane's file — the
signature change is pre-approved here, only the callers below need adding).

REPLACE:

```rust
fn jsse_cipher_suite_name(rustls_name: &str) -> String {
```

WITH:

```rust
pub(crate) fn jsse_cipher_suite_name(rustls_name: &str) -> String {
```

**File:** `native-builtins/src/t27_tls.rs`, at each of the seven sites.

REPLACE:

```rust
        .map(|cs| format!("{:?}", cs.suite()))
```

WITH:

```rust
        .map(|cs| crate::http_url_connection::jsse_cipher_suite_name(&format!("{:?}", cs.suite())))
```

(`:3418` and `:9419` carry deeper indentation and `:9011` ends in `;` — match
each site's own text.)

**Checked before nominating:** the only consumer that pattern-matches these
strings is the `auth_type` guess at `t27_tls.rs:9067`
(`Some(s) if s.contains("ECDSA")`), which is unaffected — no TLS 1.3 suite name
contains `ECDSA` under either spelling.

**Not folded into this lane's edit** because those seven strings are also
written into registry entries other lanes are actively editing, and a
seven-site rename in a file this lane does not own is exactly the change that
should be one reviewable commit of its own.

## How to verify

Registry first — the six accessors must own their slots on **both** carrier
classes, per C6-1's warning that `register_one` in this file overwrites
`net_phase_e.rs` for the shared request surface:

```
cratonvm --jdk-only --dump-native-registry reg-after.json -cp . <Main>
```

**Flag order is load-bearing**: report flags after the main class name are
ignored silently (no file, no warning, exit 0).

Behaviourally, against the embedded-Tomcat HTTPS fixture from
`P4A-TOMCAT-20260812.md` §2:

| | before this change | HotSpot 25 | after (PREDICTED) |
|---|---|---|---|
| `getSSLSession()` after `connect()` | `IllegalStateException` (accessors landed, producer absent) | present | present |
| `getCipherSuite()` | `IllegalStateException` | `TLS_AES_256_GCM_SHA384` | the negotiated suite, **JSSE-spelled** |
| `getServerCertificates()` | `IllegalStateException` | 1 certificate | the peer chain, leaf first |
| `getPeerPrincipal()` | `IllegalStateException` | the leaf's subject | the leaf's subject |
| all six before `connect()` | `IllegalStateException` | `IllegalStateException: connection not yet open` | unchanged |
| a request whose hostname check FAILS, then `getCipherSuite()` | `IllegalStateException` | the suite (the session exists) | the suite |

**That last row is the one that distinguishes a correct capture from one placed
below STEP 1** — and note the polarity: a capture placed WRONGLY passes that row
and fails all the others. Run the successful-request rows first.

Also re-run `SkipSslVerificationHttpRequestFactory` (Spring) and
`TestCustomSslTrustManager.testCustomTrustManagerNone` (Tomcat,
`P4A-TOMCAT-20260812.md` §8a(ii)) — the second is the divergence that motivated
the carrier swap, and it needs re-measuring now that CratonVM is on
HotSpot's class. **A pass there is still not evidence** until someone
establishes what the `None` case is supposed to enforce.

## Residuals

1. **`getLocalCertificates()` / `getLocalPrincipal()` remain unexercised.** No
   client-auth request has run through this path. Carried from C6-1 residual 1
   unchanged — this lane added a producer, not a client-auth test.
2. **The table is never evicted.** Bounded by connections made, not by time.
   Carried from C6-1 residual 2.
3. **`jsse_cipher_suite_name` is a prefix rewrite, not a table.** It is exact for
   every suite rustls can negotiate today (five `TLS13_*`, everything else
   already registry-named). If rustls ever adds a variant whose name diverges
   some other way, this returns it unchanged and the divergence is silent — the
   same failure mode, one suite narrower.
4. **The protocol string is derived from `protocol_version()` with a `_ =>
   "TLSv1.3"` fallback** at the call site. A connection that somehow negotiated
   neither 1.2 nor 1.3 would be recorded as 1.3. Pre-existing; not touched.
