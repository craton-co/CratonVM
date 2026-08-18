# G57-1 — the endpoint the carrier never carried

**Status:** MEASURED (see the banner below; the body is preserved as written).
**Provenance:** the "before" rows are G51-1's, MEASURED on `9ae371468`
(`target-rel3`) and not re-run here. The change itself is compile-clean and
unit-green (§3); the four vector rows are **PREDICTED** until the binary at
`e7e840264`+1 runs `RSslLiveSession`, and are marked so everywhere below.
Done in-session by the orchestrator, not by a lane.

This closes **G51-1 N1**, which had been open across four lanes for the same
reason each time: the reader was in one file, the writers in two others, and
no lane owned all three.


> **MEASURED 2026-08-17 — all four rows are green, and so is the vector.**
> Binary `C:/craton/target-rel6/release/cratonvm.exe` from `3fcc8d90f`,
> `--jdk-only` against HotSpot 25.0.3+9-LTS:
>
> ```text
> RSslLiveSession   104 checks, 0 failing   diff against the oracle is EMPTY
> ```
>
> `client.peerHost` and `attrs.shadow.peerHost` answer `localhost`; both
> `peerPort` rows answer the server port. The PREDICTED column below was right,
> and the full `--jdk-only` arm is **98 of 99** with `RSslLiveSession` among the
> passes — so nothing else moved to buy it.

---

## 0. The headline

| | before (MEASURED, `9ae371468`) | after (PREDICTED) |
|---|---|---|
| `RSslLiveSession` | 95 checks, **14 failing** | **10 failing** |
| `client.peerHost` | `null` | `localhost` |
| `client.peerPort.isServerPort` | `false` | `true` |
| `attrs.shadow.peerHost` | `null` | `localhost` |
| `attrs.shadow.peerPort.isServerPort` | `false` | `true` |

Four rows, one fix — both pairs are the same object. `RSslLiveSession.attrs()`
re-asks `s1`, the session `handshake()` already asserted, with the attribute
map now populated so the E31-1 §2 trap is armed.

## 1. Why the answer had nowhere to come from

`getPeerHost()`/`getPeerPort()` on an HTTPS client session have **three**
possible sources, and G51-1 measured all three shut:

| source | why it cannot answer |
|---|---|
| the session's own slots | width 4; there is no slot for an endpoint, and G44-1 §4 refused to widen the shape because `RSslNullSession` pins its width |
| the `session_stream_id` -> `servlet::s2_tls_session_info` fallback | slot 2 is `HTTPS_CLIENT_SESSION_MARKER`, a constant **chosen so that every socket-registry lookup misses** — its own doc comment explains that this connection cannot be given a real `servlet` TLS id without leaking one registry entry per request. It misses BY DESIGN |
| the peer certificate, or SNI | **measured wrong.** G51-1 §1: a request to `https://127.0.0.1/` has leaf subject `CN=localhost` and SNI `localhost`, and HotSpot answers `127.0.0.1` |

That is why the side table is the only option rather than the tidy one, and
why this record does not propose deriving the host from anything on the wire.

`session_peer_endpoint_table` and `record_session_peer_endpoint` landed in
`206043653` (G51-1). The reader was already wired into `getPeerHost` and
`getPeerPort`, after the `>= 6` width branch and before the fallback, with
both halves of that order pinned by a test. **Nothing was missing except a
caller that knew the endpoint.**

## 2. Why the caller did not know it

`https_session_object` mints at the **first accessor call**. By then `perform`
has returned and its `Url1` is gone. G51-1 checked and refused a "last dialled
endpoint" latch in `t27_tls::client_config_for_host` for a measured reason:
`RSslLiveSession`'s own `distinct` family opens a second connection before
re-reading the first session, so a latch would report one connection's
endpoint for another's — and that call site passes the host without a port
anyway.

So the endpoint has to be **carried**, from the handshake to the mint, on the
per-carrier entry that already carries the protocol, the cipher and the peer
chain. `HttpsCarrierSession` gains `peer_host: String` and `peer_port: i32`;
`record_https_carrier_session` gains the two parameters;
`huc_verify_hostname` — the one function in the VM holding the carrier, the
protocol, the cipher, the chain and now the endpoint at one instant — gains
`port: u16` and passes `parsed.port`.

**The port is the resolved one.** `parse_url` fills the scheme default, so a
plain `https://h/p` records `443` — the port the connection actually dialled,
which is what `HttpsURLConnection` answers. Confirmed at
`http_url_connection.rs`'s `default_port` binding, not assumed.

The fallback minter `huc_mint_verifier_session` gets the same line. No row on
`RSslLiveSession` reaches it — by construction a session minted there is not
the one `getSSLSession()` answers with, which is what G44-1 fixed — but a
verifier handed that session must not be told `null`/`-1` merely because the
carrier was unavailable.

## 3. What is guarded, and what a green test does NOT prove

Two new tests, both green (`cargo test -p cratonvm-native-builtins --lib`,
11 passed in the endpoint family, 0 failed):

* **`a_recorded_handshake_carries_the_endpoint_the_url_named`** — behavioural.
  Records `127.0.0.1`/`45123` and reads both back. Its assertion message
  carries the IP-literal counter-example, so a future reader who "simplifies"
  the host to the certificate subject is told why that is wrong at the point
  of failure. **This is also what stops the two new fields being dropped as
  unused** — nothing else in the crate reads them under `cfg(test)`.
* **`the_minted_session_records_its_endpoint`** — source witness, the idiom
  `the_session_capture_precedes_the_success_path_early_return` already uses in
  the sibling file. It asserts the call is present in `https_session_object`
  AND that no allocation sits between the session's re-read from its pin and
  the call. A `record_session_peer_endpoint` on a stale `ObjectRef` would key
  the table on a vacated from-space address and answer nothing — which is
  **indistinguishable at the vector from the bug this closes**, and is the
  failure mode a behavioural test here could not see.

`https_session_object` cannot be tested behaviourally from the mock context:
it allocates a real `javax/net/ssl/SSLSession` through
`try_alloc_concurrent_synthetic` and publishes it under a global root, neither
of which `MockNativeContext` models. The source witness is a second-best and
is labelled one.

**A green unit test is not the four rows.** Per this directory's standing
rule, the "after" column of section 0 stays PREDICTED until the binary runs
the vector.

## 4. What must not move

* `record_session_peer_endpoint` is a **no-op on an empty host AND a
  non-positive port**, deliberately: the readers FALL THROUGH an absent row to
  `session_stream_id`, and a row of `("", -1)` would shadow an answer the
  socket registry could still give. Any future call site that cannot name an
  endpoint must keep relying on that, not pass placeholders.
* The reader order — width branch first, then the table, then the fallback —
  is pinned by `a_recorded_endpoint_does_not_shadow_the_slots_that_carry_one`
  and is not touched here.
* `NEW13_SSL_SESS_FIELDS` is unchanged. The session shape stays width 4, so
  `RSslNullSession` (PASS, 89 checks) is not revalidated by this change.

## 5. What is still open on `RSslLiveSession`

Ten rows, PREDICTED, all previously recorded:

* `client.sslSession.sameObjectTwice`, `verifier.sameObjectAsGetSSLSession` —
  fixed in `aed6a3b73`/`860285255`, never yet measured in a binary containing
  both.
* the four `server.*` rows — fixed in `206043653`, same caveat.
* the four `drain.conn.*` rows — **G51-1 N2**, untouched: a `BaisEvent` hook in
  `native-api/src/registry.rs` + `native-io/src/lib.rs`, mirroring the
  `BaosEvent` one both files already carry.

If the pending build measures the first six as closed, the drain family is the
whole of what is left on this vector.

## 6. NOMINATIONS

**N1 — G51-1 N3, unchanged and still not taken.** `TlsServerStreamEntry`
should carry the peer address the way `TlsClientStreamEntry` does;
`rustls_server_accept_within` drops it as `_peer`. Two lines. The hole it
closes (`try_clone` failure at accept time) has never been produced by any
measurement in this tree, which is why it stays a nomination.

**N2 — G51-1 N4, unchanged.** The SSLEngine session's post-handshake endpoint
has no row on any vector. Section 1 of G51-1 MEASURED the contract; the fix is
`record_session_peer_endpoint` from `engine_session_for`, gated on handshake
completion. It needs a `regression-suite/src/` change first, so the row exists
before the fix does.
