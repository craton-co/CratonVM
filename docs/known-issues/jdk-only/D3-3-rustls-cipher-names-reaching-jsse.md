# D3-3 — the rustls cipher-suite spelling: five of the seven sites need it, one does not, and the count of callers is the interesting number

**2026-08-13, lane D3.** Applies wave D's third queue entry
(`WAVE-D-QUEUE.md` row 3, from `C12-2` NOM 2). The patch below is apply-ready and
verified byte-for-byte against the working tree.

**This lane may not write `.rs` and may not build or run the VM.** The rustls
facts are read from `rustls-0.23.38` (the version `Cargo.lock` resolves) in the
local registry; the JSSE facts are C12-2's measurements on this host; the patch
shape was type-checked with plain `rustc` on a self-contained extract (§4).

---

## 0. Verdict, up front

| site | function | where the string goes | needs the JSSE spelling? |
|---|---|---|---|
| `:3254` | `rustls_client_connect` | `TlsClientStreamEntry::negotiated_cipher` → `rustls_session_info` → `SSLSession` slot 1 | **YES** |
| `:3418` | `rustls_server_accept` | `TlsServerStreamEntry::negotiated_cipher` → same | **YES** |
| `:3601` | `rustls_server_handshake_over_stream` | same | **YES** |
| `:3676` | `rustls_client_handshake_over_stream` | same | **YES** |
| `:9419` | `build_synthetic_ssl_session` | `SSLSession.getCipherSuite()` / `getHandshakeSession()`, directly | **YES** |
| `:5898` | `run_loopback_self_test` | a diagnostic string from `cratonvm.tls.T27SelfTest.run()` | **no — cosmetic only** |
| `:9011` | `engine_take_pending_trust_check` | never leaves Rust; sole consumer is `contains("ECDSA")` | **NO — provable no-op** |

C12-2 NOM 2 nominated all seven. Its own "Checked before nominating" paragraph
names the `:9067` `auth_type` consumer as unaffected — **without noticing that
this is precisely what makes `:9011` unnecessary.** §2 closes that.

## 1. The five that need it, traced to the Java-visible read

`format!("{:?}", cs.suite())` prints rustls's `CipherSuite` VARIANT name.
rustls spells every TLS 1.3 suite `TLS13_AES_256_GCM_SHA384`; the IANA registry —
and therefore JSSE — spells the same suite `TLS_AES_256_GCM_SHA384`. Measured by
C12-2 against HotSpot 25.0.3+9-LTS: `JSSE supports TLS_AES_256_GCM_SHA384 = true`,
`JSSE has any TLS13_* name = false`. TLS 1.3 is this client's default.

`:3254`, `:3601` and `:3676` write `TlsClientStreamEntry::negotiated_cipher`
(`t27_tls.rs:1485`); `:3418` writes `TlsServerStreamEntry::negotiated_cipher`
(`:1496`). Both are read by exactly one function,
`rustls_session_info` (`:4220`, tuple element 1), whose consumers are:

* `t27_tls.rs:4915` — `SSLServerSocket.accept()`, which writes it into
  `javax/net/ssl/SSLSession` slot 1;
* `phases_late/ssl_security.rs:1707` — the shared `SSLSession` builder;
* `phases_late/ssl_security.rs:2867` — `new13_finish_socket`, into
  `NEW13_SESS_CIPHER`.

All three are the value `SSLSession.getCipherSuite()` returns.
**Two independent tells confirm the consumer expects the registry name:**
`ssl_security.rs:2882`'s fabricated fallback is the literal
`"TLS_AES_128_GCM_SHA256"`, and `:9419`'s own `unwrap_or_else` is the literal
`"TLS_AES_256_GCM_SHA384"` — both JSSE spellings, sitting in the same expression
as a `{:?}` that produces the other one. The round trip does not close either:
`t27_tls::java_cipher_name_to_suite`, the VM's own reverse mapping, has never
accepted a `TLS13_` name.

## 2. `:9011` — why the change is a provable no-op, and should not be made

`engine_take_pending_trust_check` (`:8970`) stores the name in
`PendingTrustCheck::negotiated_cipher_suite_name`. That field has **four**
mentions in the whole tree (`:8945` declaration, `:9016` write, `:9383` a
`None` literal, `:9067` read) and exactly one reader:

```rust
    // Real JSSE authType is the key-exchange/signature algorithm; we don't
    // track it precisely, so derive a best-effort guess from the negotiated
    // cipher suite name. …
    let auth_type = match pending.negotiated_cipher_suite_name.as_deref() {
        Some(s) if s.contains("ECDSA") => "ECDSA",
        _ => "RSA",
    };
```

The only thing that reaches Java is `"ECDSA"` or `"RSA"`, as the `authType`
argument to `checkServerTrusted`/`checkClientTrusted`. `jsse_cipher_suite_name`
rewrites `TLS13_x` → `TLS_x` and nothing else, so it can only change a TLS 1.3
name, and no TLS 1.3 suite name contains `ECDSA` under either spelling.
`s.contains("ECDSA")` is therefore invariant under the rewrite — asserted
mechanically in §4's `rustc` run, not argued.

So applying it here changes no behaviour and adds a "this value is a JSSE cipher
name" claim to a value whose actual job is "a rustls suite name used to guess an
auth type". **Leave it.** The patch instead adds one line of comment there — no,
it does not: this record is the place that answer lives, and touching the site
would only invite the next reader to re-derive it. It is named here and in the
helper's doc comment.

## 3. `:5898` — a diagnostic, offered as optional

`run_loopback_self_test` returns
`format!("OK proto={} cipher={} alpn={}", …)`, handed to Java by
`cratonvm/tls/T27SelfTest.run()Ljava/lang/String;` (`:5760`-`:5777`) — a
CratonVM-private diagnostic class, not a JSSE API. Nothing is contracted about
the spelling. The in-tree unit test `t27_loopback_self_test` (`:6324`) asserts
`starts_with("OK ")`, `contains("proto=TLSv1.3")` and `contains("alpn=h2")` —
**it does not assert the cipher substring**, so changing it breaks nothing.

Recommendation: apply it, as **T6**, so a human reading the self-test sees the
same name the API reports. It is cosmetic and it is the last `{:?}` in the file,
which is worth something on its own: after T1–T6 the string
`format!("{:?}", cs.suite())` appears exactly once in `t27_tls.rs`, inside the
one helper. If the orchestrator prefers a strictly minimal change, drop T6 and
the patch is still correct.

## 4. HOW MANY CALLERS THERE REALLY ARE

The brief asked for the count. `grep -rn "cs.suite()"` over first-party code
(excluding `native-builtins/vendor/`, which is a vendored rustls fork, and
`examples/`):

| producer | file | status before this patch |
|---|---|---|
| 1 | `http_url_connection.rs:2723` | **already correct** — the one caller `jsse_cipher_suite_name` has |
| 2–6 | `t27_tls.rs:3254 :3418 :3601 :3676 :9419` | wrong, and Java-visible as a cipher name |
| 7 | `t27_tls.rs:5898` | wrong, but a diagnostic |
| 8 | `t27_tls.rs:9011` | never leaves Rust |

**Eight producers of a rustls suite name. Six of them hand it to Java as a cipher
suite name, and exactly one of those six used the helper.** `jsse_cipher_suite_name`
otherwise has six *test* callers (`http_url_connection.rs:4381`-`4402`) and no
others — a helper better covered by its unit tests than by its callers.

This is the eighth instance this session of *"a correct helper exists and the
callers do not use it"*. The interesting variation is that here the helper was
written in the same commit as its single caller and never offered to the others:
C12-2 §2 fixed the site it was standing on and left the seven it was not, with
the nomination attached. **The pattern is not "someone forgot the helper" — it is
"the helper was scoped to the bug being fixed, and the family was named in a
document instead of in code."** The patch's answer is to give `t27_tls.rs` ONE
private adapter rather than five call sites that each know the translation, so
the count of places that can drift is 2 (one per file) instead of 8.

### Type-check

The patch shape was type-checked and executed with plain `rustc` on a
self-contained extract in this lane's scratch dir (stand-ins mirroring
`rustls::SupportedCipherSuite` / `suite() -> CipherSuite`, verified against
`rustls-0.23.38/src/suites.rs:66`-`77` and `src/lib.rs:566`-`568`, plus the
verbatim body of `jsse_cipher_suite_name`):

```
shape OK: TLS_AES_256_GCM_SHA384 / UNKNOWN / Some("TLS_AES_256_GCM_SHA384") / TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
```

covering `.map(negotiated_suite_name)` over `Option<SupportedCipherSuite>`, the
`.and_then(..).map(..)` chain shape, TLS 1.2 pass-through, and the `contains("ECDSA")`
invariance of §2. `negotiated_cipher_suite(&self) -> Option<SupportedCipherSuite>`
(`rustls-0.23.38/src/common_state.rs:155`) and `SupportedCipherSuite: Copy`, so
taking it by value in the adapter is free.

## 5. THE PATCH

Both files are uniformly CRLF (`t27_tls.rs`: 11,734 `\r\n` / 11,734 `\n`).
Every OLD block was verified with `str.count()` to occur **exactly once** and to
stay unique after the earlier edits in this list. **Apply T0 before T-helper**
(the helper calls what T0 makes visible), then T1–T6 in any order.

Note that the raw single line `.map(|cs| format!("{:?}", cs.suite()))` at
8-space indent occurs FOUR times (`:3254 :3601 :3676 :5898`), so T1/T3/T4 carry
several lines of trailing context purely to be unique. Those context lines are
unchanged between OLD and NEW; only the first line differs.

### T0 — `native-builtins/src/http_url_connection.rs` (`:2053`)

REPLACE:

```rust
fn jsse_cipher_suite_name(rustls_name: &str) -> String {
```

WITH:

```rust
pub(crate) fn jsse_cipher_suite_name(rustls_name: &str) -> String {
```

*(Pre-approved by that file's owning lane in C12-2 NOM 2.)*

### T-helper — `native-builtins/src/t27_tls.rs`, immediately above `TlsClientStreamEntry` (`:1469`)

REPLACE:

```rust
pub(crate) struct TlsClientStreamEntry {
```

WITH:

```rust
/// The negotiated suite's name in the spelling JSSE reports, which is what
/// `SSLSession.getCipherSuite()` is contracted to return.
///
/// `format!("{:?}", cs.suite())` prints rustls's `CipherSuite` VARIANT name,
/// and rustls spells every TLS 1.3 suite `TLS13_AES_256_GCM_SHA384` where the
/// IANA registry — and therefore JSSE — spells it `TLS_AES_256_GCM_SHA384`.
/// TLS 1.3 is this VM's default, so the raw `{:?}` name is one HotSpot never
/// produces, on essentially every connection; `java_cipher_name_to_suite` in
/// this file, the VM's own reverse mapping, has never accepted it either.
/// Measured rather than assumed — see
/// `docs/known-issues/jdk-only/D3-3-rustls-cipher-names-reaching-jsse.md`.
///
/// One function, so this file has ONE place that knows the translation. The
/// prefix rewrite itself lives in `http_url_connection` and is unit-tested
/// there in both directions.
fn negotiated_suite_name(cs: rustls::SupportedCipherSuite) -> String {
    crate::http_url_connection::jsse_cipher_suite_name(&format!("{:?}", cs.suite()))
}

pub(crate) struct TlsClientStreamEntry {
```

*(The two `—` are U+2014 EM DASH.)*

### T1 — `:3254`, `rustls_client_connect`

REPLACE:

```rust
        .map(|cs| format!("{:?}", cs.suite()))
        .unwrap_or_else(|| "UNKNOWN".to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());

    // W7-61: registry-held duplicate, taken BEFORE `stream` moves into the
```

WITH:

```rust
        .map(negotiated_suite_name)
        .unwrap_or_else(|| "UNKNOWN".to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());

    // W7-61: registry-held duplicate, taken BEFORE `stream` moves into the
```

### T2 — `:3418`, `rustls_server_accept`

REPLACE:

```rust
                    .map(|cs| format!("{:?}", cs.suite()))
```

WITH:

```rust
                    .map(negotiated_suite_name)
```

*(This 20-space-indented line is unique on its own.)*

### T3 — `:3601`, `rustls_server_handshake_over_stream`

REPLACE:

```rust
        .map(|cs| format!("{:?}", cs.suite()))
        .unwrap_or_else(|| "UNKNOWN".to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());
    // W7-61: see `TlsClientStreamEntry::raw`.
    let raw = stream.sock.try_clone().ok();
    let mut reg = sreg().lock();
```

WITH:

```rust
        .map(negotiated_suite_name)
        .unwrap_or_else(|| "UNKNOWN".to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());
    // W7-61: see `TlsClientStreamEntry::raw`.
    let raw = stream.sock.try_clone().ok();
    let mut reg = sreg().lock();
```

### T4 — `:3676`, `rustls_client_handshake_over_stream`

REPLACE:

```rust
        .map(|cs| format!("{:?}", cs.suite()))
        .unwrap_or_else(|| "UNKNOWN".to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());
    // W7-61: see `TlsClientStreamEntry::raw`.
    let raw = stream.sock.try_clone().ok();
    let entry = TlsClientStreamEntry {
```

WITH:

```rust
        .map(negotiated_suite_name)
        .unwrap_or_else(|| "UNKNOWN".to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());
    // W7-61: see `TlsClientStreamEntry::raw`.
    let raw = stream.sock.try_clone().ok();
    let entry = TlsClientStreamEntry {
```

### T5 — `:9419`, `build_synthetic_ssl_session`

REPLACE:

```rust
            .map(|cs| format!("{:?}", cs.suite()))
            .unwrap_or_else(|| "TLS_AES_256_GCM_SHA384".into());
```

WITH:

```rust
            .map(negotiated_suite_name)
            .unwrap_or_else(|| "TLS_AES_256_GCM_SHA384".into());
```

### T6 — `:5898`, `run_loopback_self_test` — **OPTIONAL, see §3**

REPLACE:

```rust
        .map(|cs| format!("{:?}", cs.suite()))
        .unwrap_or_else(|| "?".into());
```

WITH:

```rust
        .map(negotiated_suite_name)
        .unwrap_or_else(|| "?".into());
```

### NOT patched

`:9011` — see §2. It is the one site in the seven where the string is not a
cipher name in the JSSE sense, and changing it would be a no-op that misstates
what the value is for.

## 6. How to verify

There is no in-tree fixture for this; the value is only observable across a real
handshake. In order of cost:

1. **`cratonvm ... cratonvm.tls.T27SelfTest.run()`** — needs a configured
   keystore, no network. With T6 applied it prints
   `OK proto=TLSv1.3 cipher=TLS_AES_256_GCM_SHA384 alpn=h2`; without T6 it still
   prints the `TLS13_` name, which is the cheap way to tell T6 apart from T1–T5.
2. **The embedded-Tomcat HTTPS fixture** from `P4A-TOMCAT-20260812.md` §2:
   `SSLSession.getCipherSuite()` after a successful handshake must match
   `^TLS_` and must not match `^TLS13_`. That single assertion covers T1/T4
   (client) and T2/T3 (server) depending on which side the fixture inspects.
3. **`SSLEngine`** — `getSession().getCipherSuite()` / `getHandshakeSession()`
   is T5, and it is the only route to `build_synthetic_ssl_session`.
4. **The round trip that never closed**: after this patch,
   `java_cipher_name_to_suite(session.getCipherSuite())` should resolve, where
   before it returned nothing for every TLS 1.3 connection. That is a pure Rust
   assertion and is the cheapest regression guard anyone could add — this lane
   did not add it because it may not write `.rs`.

## Residuals

1. **`jsse_cipher_suite_name` is still a prefix rewrite, not a table**
   (C12-2 residual 3, unchanged). Exact for the five `TLS13_*` variants rustls
   can negotiate today; a future rustls variant that diverges some other way
   returns unchanged and the divergence is silent. Making it a table is a
   separate, larger decision.
2. **No test pins the five patched sites.** The helper's own unit tests
   (`http_url_connection.rs:4381`-`4402`) test the FUNCTION, not that anyone
   calls it — which is exactly the gap that let seven callers sit unpatched for a
   day. A source-witness test in the shape of
   `http_url_connection_tests::the_session_capture_precedes_the_success_path_early_return`
   — "`t27_tls.rs` contains no `format!(\"{:?}\", cs.suite())` outside
   `negotiated_suite_name`" — would pin the family for the cost of one function,
   and after T6 that assertion is TRUE. Strongly recommended as the follow-up;
   without T6 it must be spelled "at most one occurrence outside the helper".
3. **The protocol string was already right** (`"TLSv1.3"`/`"TLSv1.2"` is JSSE's
   spelling) and the `_ => "TLS"` fallback at four of these sites is not — JSSE
   answers `"NONE"` for an unhandshaken session, never `"TLS"`. Not measured
   against HotSpot by this lane; noted because it is the same expression, one
   field over.
