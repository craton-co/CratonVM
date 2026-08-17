# G51-1 — the peer endpoint the session never carried, and the local identity the server never reported

**Status:** PARTIAL. **Before: MEASURED** on
`C:/craton/target-rel3/release/cratonvm.exe` (`9ae371468`), 2026-08-17 — all
seven vectors, the 14 failing rows of `RSslLiveSession`, the native-registry
ownership dump, the `--jdk-only-report` violation set, and a standalone
before-state witness for the four server rows (§3). **After: NOT MEASURED, and
could not be.** This lane's brief forbids `cargo build`, `check` and `test`;
`target-rel3` is the only binary on this host (`target-rel4` never appeared,
checked twice) and it predates every change here. Every "after" below is
**PREDICTED** and labelled at each occurrence.

What *is* measured beyond the before-state:

* **The whole peer-endpoint family on the ORACLE** — HotSpot 25.0.3+9-LTS
  `Microsoft-13877124`, this host (§1). Nine shapes, including the two the
  brief asked about by name (IP literal vs hostname, and SNI) and the one that
  turned out to decide the design (the server's peer is the client).
* **That the object this lane writes its side-table rows against is the object
  the accessors read** (§3a) — measured on BOTH VMs with a probe that does not
  depend on the vector.
* **The pure logic**, extracted verbatim into `scratchpad/rs/endpoint.rs` and
  run under `rustc --edition 2021 --test` (`rustc`, never `cargo`, nothing
  written under `target/`): six tests, all passing. That is not the same as
  running it in the crate and is not claimed to be.

**Owned file:** `native-builtins/src/t27_tls.rs`, and nothing else. Everything
else is a NOMINATION in §6.

---

## 0. The headline

| | before (MEASURED, `9ae371468`) | after (PREDICTED) |
|---|---|---|
| `RSslLiveSession` | **95 checks, 14 failing rows** | **10** |
| `RSslNullSession` | PASS, 89 checks | unchanged — §5 |
| `RJdkNet` | PASS, 81 | unchanged |
| `RJdkAsyncChannel` | PASS, 141 | unchanged |
| `RJdkX509Intercept` | PASS, 26 | unchanged |
| `RCrypto` | PASS, 57 | unchanged |
| `RJdkSecurity` | PASS, 153 | unchanged — §5 |

All seven "before" rows were re-run on `target-rel3` for this lane, not taken
from G44-1. The six greens reproduced their exact check counts.

The 14 rows, MEASURED, and where each one now stands:

```text
client.sslSession.sameObjectTwice     aed6a3b73, not in this binary   -1 (G44-1, PREDICTED)
verifier.sameObjectAsGetSSLSession    G44-1 §1                        -1 (G44-1, PREDICTED)
client.peerHost                       §2 — the READER landed here;    NOMINATION N1
client.peerPort.isServerPort          §2   the two writers are one    NOMINATION N1
attrs.shadow.peerHost                 §2   line each in files this    NOMINATION N1
attrs.shadow.peerPort.isServerPort    §2   lane does not own          NOMINATION N1
server.localPrincipal                 §3 — THIS LANE                  -1 (PREDICTED)
server.localPrincipal.class           §3 — THIS LANE                  -1 (PREDICTED)
server.localCertificates.length       §3 — THIS LANE                  -1 (PREDICTED)
server.peerPort.isPositive            §3 — THIS LANE                  -1 (PREDICTED)
drain.conn.cipherSuite.raises         G44-1 §3 — NOMINATION N2 there
drain.conn.cipherSuite.message        G44-1 §3
drain.conn.sslSession.raises          G44-1 §3
drain.conn.sslSession.message         G44-1 §3
```

**The four `server.*` rows were "not investigated (G35-1 §4)" through three
lanes.** They are investigated here, they are all four in this file, and they
are all four one defect each rather than the server-session rewrite the label
suggested. That is the substantive finding of this lane; the client half is
the one the brief pointed at, and it is the half that could not be finished
from inside one file.

---

## 1. The oracle sweep, MEASURED

HotSpot 25.0.3+9-LTS `Microsoft-13877124`, this host, 2026-08-17.
`scratchpad/g51/G51Probe.java` (loopback `SSLServerSocket` + four
`SSLSocket` clients) and `scratchpad/g51/G51Engine.java` (an in-memory
`SSLEngine` pair driven to a completed TLS 1.3 handshake). Ports are
normalised to symbols in the transcript so the probe is re-runnable.

```text
                                              getPeerHost()   getPeerPort()
never-connected SSLSocket                     null            -1
client SSLSocket dialled by hostname          localhost       the server port
client SSLSocket dialled by IP literal        127.0.0.1       the server port
  ... the SAME with SNIHostName("localhost")
      explicitly set on the socket            127.0.0.1       the server port
the SERVER's view of that handshake           127.0.0.1       the CLIENT's
                                                                ephemeral port
the SERVER's view, client sent SNI            127.0.0.1       (as above)
  (and getRequestedServerNames() = localhost on that same session)
SSLEngine, no peer named                      null            -1
SSLEngine("example.test", 8443), pre-hs       null            -1
SSLEngine("example.test", 8443), post-hs      example.test    8443
SSLEngine SERVER side of that handshake       null            -1
after invalidate(), every row above           unchanged       unchanged
```

Four of those rows are load-bearing and none of them was assumed:

1. **The host is the one the CALLER NAMED.** The IP-literal row settles it in
   the only way that can be settled: the leaf certificate's subject is
   `CN=localhost`, the SNI sent was `localhost`, and the answer is `127.0.0.1`.
   Neither the certificate nor SNI reaches `getPeerHost()`. G44-1 §4's note
   ("do not verify one against the other") is confirmed and now has the
   counter-example that proves it rather than the coincidence that hid it —
   in G44-1's fixture both happened to read `localhost`.
2. **SNI does not reach it from the server side either.** The same session
   answers `getRequestedServerNames() = localhost` and `getPeerHost() =
   127.0.0.1` in one call sequence. The accept path in `t27_tls` has
   `rustls_session_info`'s SNI in hand and it would have been the obvious
   thing to write into the host; it is the wrong answer.
3. **The server's peer is the CLIENT.** Its port is ephemeral and is NOT the
   listener's, which is why `RSslLiveSession` asserts `server.peerPort
   .isPositive` and `client.peerPort.isServerPort` — two different questions,
   and a fix that answered the server row with the listener's port would pass
   the vector and be wrong.
4. **`invalidate()` changes neither.** So the endpoint is not part of the
   validity state and must not be evicted with it.

The engine rows are recorded for the next lane and are NOT acted on here:
`SSLEngine.getPeerHost()` (the engine's own advisory accessor) already answers
`example.test` before the handshake, while the *session's* `getPeerHost()`
answers `null` until the handshake completes and `example.test` after. That is
a real divergence surface — `RSslNullSession` pins the pre-handshake half at
`null` and nothing in the tree pins the post-handshake half. **N4.**

---

## 2. The client rows: the reader landed, the writers did not

MEASURED, `9ae371468`:

```text
CK RSslLiveSession client.peerHost                    = null   WANT localhost
CK RSslLiveSession client.peerPort.isServerPort       = false  WANT true
CK RSslLiveSession attrs.shadow.peerHost              = null   WANT localhost
CK RSslLiveSession attrs.shadow.peerPort.isServerPort = false  WANT true
```

Both pairs are the SAME object: `RSslLiveSession.attrs()` re-asks `s1`, the
session `handshake()` already asserted, with the attribute map now populated so
the E31-1 §2 trap is armed. One fix, four rows.

**The shape G44-1 §4 nominated is built and it is in place.**
`session_peer_endpoint_table` is a side table keyed on the session OBJECT —
`gc_stable_objref_key`, the same key `session_peer_certs_table` uses to solve
the identical problem (a fact about the peer that the narrow shape has no slot
for) for the certificate chain. `record_session_peer_endpoint` is `pub(crate)`
precisely so the two nominated writers are one line each. `getPeerHost` and
`getPeerPort` consult it **after** the `>= 6` width branch and **before** the
`session_stream_id` → `servlet::s2_tls_session_info` fallback, and both halves
of that order are pinned by a test.

**Why the fallback cannot answer for this shape, restated because it is what
makes the side table the only option rather than the tidy one.** The HTTPS
client session is width 4 and its slot 2 is
`net_phase_e::HTTPS_CLIENT_SESSION_MARKER`, a value chosen *so that every
socket-registry lookup misses* — that constant's own doc comment explains why
this connection cannot be given a real `servlet` TLS id without leaking one
registry entry per request. So the fallback misses BY DESIGN, and no amount of
work on it will produce an answer.

**The rows are still red, and this lane is not claiming otherwise.** The two
writers are:

* `net_phase_e::https_session_object`, which has the carrier and can read the
  URL's host and port;
* `http_url_connection::huc_mint_verifier_session`, the fallback minter G44-1
  §1 kept for the `connection == None` and refused-allocation states.

Both files are explicitly not this lane's, and both were being edited
concurrently. **N1.**

**What was considered and rejected rather than left unsaid.** A "last dialled
endpoint" latch inside `t27_tls::client_config_for_host` — which
`http_url_connection::perform` DOES call, on the requesting thread, with
`parsed.host` in hand — was checked and refused. `record_client_peer_chain`
runs from `https_session_object`, i.e. at the first ACCESSOR call, arbitrarily
long after `perform` returned and after other connections have run through the
same latch; `RSslLiveSession`'s own `distinct` family opens a second connection
before re-reading the first session. A latch would have turned four rows green
on this vector and reported one connection's endpoint for another's session,
which is the shape this directory exists to refuse. It also has no port: that
call site passes the host only.

---

## 3. The four `server.*` rows — investigated, and all four are here

Nobody had looked at these. MEASURED, `9ae371468`:

```text
CK RSslLiveSession server.localPrincipal           = null  WANT CN=localhost
CK RSslLiveSession server.localPrincipal.class     = null  WANT javax.security.auth.x500.X500Principal
CK RSslLiveSession server.localCertificates.length = -1    WANT 1
CK RSslLiveSession server.peerPort.isPositive      = false WANT true
```

They are two defects, not four, and they meet at one object: the width-4
`SSLSession` that `SSLServerSocket.accept()` mints and stashes on the accepted
socket.

### 3a. First, the question that had to be settled before either fix could be believed

`--jdk-only-report` on this vector reports exactly one `SSLSession`-related
violation, and it is the one that matters here:

```json
{"kind": "native-shadows-bytecode",
 "summary": "bridge native shadows bytecode of javax/net/ssl/SSLSocket.getSession()Ljavax/net/ssl/SSLSession;",
 "class": "javax/net/ssl/SSLSocket", "method": "getSession", "native_kind": "bridge"}
```

That native is `ssl_security.rs:4020`, `owns_slot=true`, and it does NOT return
the accept path's session unconditionally: it calls
`new13_resolve_socket_session`, which reads slot `NEW13_SOCK_SESSION` and, on a
miss, **allocates a fresh session** from `new13_alloc_ssl_session(tls_id)` and
writes it back. `NEW13_SOCK_SESSION` and `t27_tls`'s `SSS_SOCK_SESSION` are
both slot `4`, so the two agree *if the write lands* — and this file already
carries a measured record (the `sslserversocket-accept-stream-id` fix) of a raw
`ctx.set_field` on this very object being **silently dropped** because
`javax/net/ssl/SSLSocket` is a real loaded class with a real layout. A side
table keyed on an object nothing ever reads back is a fix that changes nothing
and looks like it worked.

The two paths produce sessions that are indistinguishable through every row
`RSslLiveSession` asserts: same width, same slot 2 (`RUSTLS_SOCK_ID_BASE +
rid`), and `new13_alloc_ssl_session` reaches the SAME `rustls_session_info` for
protocol and cipher. So the vector cannot answer this and neither can any
existing green row.

`scratchpad/g51/G51Stash.java` can: if the stash is dropped, every
`getSession()` re-mints, because the write-back is dropped too. MEASURED,
2026-08-17, one loopback handshake, both VMs:

```text
HotSpot   G51STASH sameObject=true cipher=true idEq=true localCerts=1     localPrincipal=CN=localhost peerHost=127.0.0.1 peerPort=55543
CratonVM  G51STASH sameObject=true cipher=true idEq=true localCerts=null  localPrincipal=null         peerHost=null      peerPort=-1
```

`sameObject=true` on CratonVM settles it: the accept path's object is the
object the accessors read, and a row keyed on it is reachable. The same line is
an independent before-state witness for all four rows — including
`server.peerHost`, which `RSslLiveSession` does **not** assert and which HotSpot
answers `127.0.0.1`, matching §1.

### 3b. `server.peerPort.isPositive` — the accepted peer's address was thrown away

`rustls_server_accept_within` binds the accepted peer address as `_peer` and
drops it, and `TlsServerStreamEntry` — unlike its client twin
`TlsClientStreamEntry`, which carries `peer_host`/`peer_port` — has no field
for it. So `getPeerPort` fell to `session_stream_id` →
`servlet::s2_tls_session_info`, which reads the **native-tls** stream table,
while an accepted rustls stream lives in this file's own `server_streams`. A
guaranteed miss, and `-1`.

`rustls_server_peer_endpoint` asks the duplicate socket handle the entry
already keeps for exactly this class of out-of-band question (`raw`, added for
`rustls_stream_close`), so no struct grows for one accessor. The IPv4-mapped
normalisation is split out as `socket_addr_endpoint` because it is the only
part with a rule in it and a rule that lives inside a function needing a live
TLS peer is a rule nothing checks.

### 3c. The three `server.local*` rows — a writer named in a doc comment and absent from the tree

`ssl_security`'s `getLocalCertificates` and `getLocalPrincipal` both read
`t27_tls::local_certs_for_session`, and the second derives its subject from the
leaf of what the first returns — which is HotSpot's own contract, so the three
rows are one fact. `session_local_certs_table`'s doc comment has named
`record_local_cert_chain` as its crate-visible writer since the table was
written. `grep -rn 'record_local_cert_chain' native-builtins/src/` returned
**exactly one hit: that doc comment.** The function did not exist. The table
had one writer, an open-coded `.lock().insert(..)` at the tail of
`build_synthetic_ssl_session` — a function `SSLServerSocket.accept()` never
reaches.

`record_local_cert_chain` now exists, `build_synthetic_ssl_session` is routed
through it (so the two writers cannot drift), and the accept path calls it with
the listener's own chain, read from `sss_listener_identities` — the very
identity the rustls `ServerConfig` was built out of, so the chain reported and
the chain presented on the wire have one source.

**The empty-chain contract is load-bearing and is preserved exactly.**
`client.localCertificates = null` and `client.localPrincipal = null` are
measured GREEN rows for a client with no configured identity, and both readers
answer `null` only on an EMPTY chain. `record_local_cert_chain` early-returns on
an empty vector, which is what `if !local_chain.is_empty()` did before it; a
writer that recorded an empty vector would turn those two greens into
`Certificate[0]` and a `CN=Unknown` principal.

### 3d. Why the predicted values are what they are, without a build

`server.localCertificates.length = 1` and `server.localPrincipal = CN=localhost`
rest on a chain that is already measured rather than assumed. The vector's
client trusts exactly one certificate, the vector's self-signed
`CN=localhost` leaf; the handshake COMPLETES (every other `server.*` row is
green); therefore the server presented that leaf and only that leaf; therefore
`create_ssl_server_socket` took the `configured` identity out of the
`SSLContext` rather than the `require_runtime_tls_identity()` fallback (which
would have presented a certificate the client does not trust and failed the
handshake); therefore `identity.cert_pem` — the string this lane parses — is
that leaf. `basic_der_extract_names` on it is the same call that already
answers `client.peerPrincipal.name = CN=localhost`, green today, and the
`X500Principal` mirror is the same 1-field synthetic `getPeerPrincipal` builds
for `client.peerPrincipal.class`, also green.

**After: PREDICTED, 4 rows, 14 → 10.**

---

## 4. The GC hazard that was already in the code being edited

The accept path held `session` — and the string `p` — raw across
`ctx.create_string`, which can run a moving young collection:

```rust
let session = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 4)?;
let p = ctx.create_string(&proto);
let c = ctx.create_string(&cipher);   // <- p and session both live across this
ctx.set_field(session, 0, Value::Object(Some(p)));
```

Pre-existing, and this lane's two recording calls would have been a third and
fourth consumer of the same stale reference. The session is now pinned (after
`sock_pin`, so the single existing `unpin_native_roots(sock_pin)` still
truncates both) and re-read before each write, and `p` is written before `c` is
allocated. Same discipline `new13_finish_socket` and `net_phase_e
::https_session_object` already use. **Rows: zero** — a moving collection at
those two points is what makes it observable and nothing in the suite forces
one. Recorded rather than done silently because it is a change to a code path
this lane's rows depend on.

---

## 5. What must not move, checked row by row

| green row today | reads | still green after? |
|---|---|---|
| `client.localCertificates` / `client.localPrincipal` (null) | `local_certs_for_session` | yes — `record_local_cert_chain` keeps the empty-chain early return verbatim, and nothing records a chain for a client session |
| `RSslNullSession`'s `getPeerHost`/`getPeerPort` (null / -1), incl. `attrs.shadow.*` | width-4 fallback | yes — nothing records an endpoint for an engine or null session; the ABSENT row falls through, which is why "nothing to say" is deliberately not written as a blank row |
| `RSslNullSession`'s `getLocalCertificates`/`getLocalPrincipal` (null) | `build_synthetic_ssl_session` | yes — that call site's behaviour is unchanged, only its spelling |
| `RJdkSecurity` `engine.getPeerHost()` / `getPeerPort()` | `SSLEngine`'s own accessors, not the session's | yes — untouched |
| `RSslLiveSession` `server.getId.length` / `.isValid` / `.cipherSuite` | the accept-minted session's slots 0–2 | yes — same values, written through a pin |
| `RSslLiveSession` `server.peerPrincipal.raises` / `.message` | `peer_certs_for_session`, a DIFFERENT table | yes — the local chain is not the peer chain, and the anonymous-client refusal is what separates them |
| `RForeignLayoutJdkInterfaces`'s SENTINEL `SSLSession` | the application's own bytecode | yes |

`grep -l 'SSLServerSocket\|getLocalCertificates\|getLocalPrincipal\|getPeerHost\|getPeerPort'` over `regression-suite/src/*.java` returns four files, all four in the
table above. `RSslLiveSession` is the only vector in the suite that reaches
`SSLServerSocket.accept()` at all.

**No registration is added, moved or removed.** `--dump-native-registry` on
`9ae371468` confirms `getPeerHost` (`t27_tls.rs:18135`) and `getPeerPort`
(`:18163`) are the sole registrations of either name and both `owns_slot=true`;
`getLocalCertificates` (`ssl_security.rs:5676`) and `getLocalPrincipal`
(`:5640`) own theirs and are reached through this file's tables rather than
re-registered here. G44-1 §5's registrar collapse is **not** taken and
`this_files_registrar_owns_five_of_the_six_https_session_accessors` is
untouched: collapsing it deletes `net_phase_e`'s `getSSLSession` body, which is
the one body both remaining identity assertions go through, and this lane can
run the vector but cannot build a binary to run it against — which is exactly
the precondition G44-1 N5 states.

`the_only_rustls_suite_spelling_left_is_the_adapters_own` is unaffected: no
`format!("{:?}", cs.suite())` is added; nothing in this change names a cipher
suite.

---

## 6. NOMINATIONS

**N1 — `native-builtins/src/net_phase_e.rs` and
`native-builtins/src/http_url_connection.rs`: two one-line writers. 4 rows.**
The reader, the table and the `pub(crate)` entry point are all in place; what
is missing is a caller that knows the endpoint. **Change:** in
`net_phase_e::https_session_object`, beside the existing
`crate::t27_tls::record_client_peer_chain(ctx, session, ..)`, add
`crate::t27_tls::record_session_peer_endpoint(ctx, session, host, port)` with
the carrier's URL host and port; the same one line in
`http_url_connection::huc_mint_verifier_session`, beside ITS
`record_client_peer_chain`. Both call sites already hold the session and both
already call into this file at exactly the right moment. **MEASURED targets:**
`getPeerHost() = localhost`, `getPeerPort() = <the server port>`. **Do not
derive the host from the certificate or from SNI** — §1 measures both as wrong,
with the IP-literal case as the counter-example. The port must be the one the
URL named; for a default-port `https:` URL with no explicit port that is `443`,
which is what `HttpsURLConnection` connected to.

**N2 — the four `drain.conn.*` rows.** Unchanged from G44-1 N2: a `BaisEvent`
hook in `native-api/src/registry.rs` + `native-io/src/lib.rs`, the mirror of the
`BaosEvent` one both files already carry. Not this lane's files and not
re-investigated here.

**N3 — `native-builtins/src/t27_tls.rs` (this file), for a lane that can
build: `TlsServerStreamEntry` should carry the peer address the way
`TlsClientStreamEntry` does.** `rustls_server_peer_endpoint` asks the `raw`
socket handle instead, which is correct and cheap but has one hole: `raw` is
`None` when `try_clone` failed at accept time, and the endpoint is then lost
for the life of the stream. `rustls_server_accept_within` has the address in
hand — it is the `_peer` it currently drops — and storing it is two lines.
Deliberately not taken here because it widens a struct on a path this lane
cannot execute, for a case (`try_clone` failure on a freshly accepted loopback
socket) that no measurement in this tree has ever produced.

**N4 — the SSLEngine session's post-handshake endpoint has no row anywhere.**
§1 MEASURED: `createSSLEngine("example.test", 8443)` gives a session that
answers `null`/`-1` before the handshake and `example.test`/`8443` after, while
the ENGINE's own `getPeerHost()`/`getPeerPort()` answer `example.test`/`8443`
throughout. `RSslNullSession` pins the pre-handshake half (and pins that the
session must NOT answer the engine's advisory host, which is the trap in this
family); nothing pins the post-handshake half, and `RSslLiveSession` reaches no
engine. The vector work is a `regression-suite/src/` change, not this lane's;
the fix, if it is one, is `record_session_peer_endpoint` from
`engine_session_for` with the engine's recorded advisory peer — this file
already keeps one (see the `SSLEngine.getPeerPort()` side table), so it is a
reader away, but it must not fire before the handshake completes.

**N5 — G44-1 N5 re-raised unchanged**, with its precondition now sharper: this
lane could RUN `RSslLiveSession` and still could not take the collapse, because
running the vector proves a replacement `getSSLSession` body equivalent only
if the binary contains it. The precondition is build **and** run, not run.

---

## 7. What changed, and how it is guarded

`native-builtins/src/t27_tls.rs`:

| change | rows |
|---|---|
| `session_peer_endpoint_table` + `record_session_peer_endpoint` + `session_peer_endpoint`, keyed on the session object exactly as `session_peer_certs_table` is | §2, 0 here — 4 with N1 |
| `getPeerHost`/`getPeerPort` consult it after the `>= 6` width branch and before the `session_stream_id` fallback | §2 |
| `record_local_cert_chain`, the writer the table's doc comment has named since it was written; `build_synthetic_ssl_session` routed through it | §3c, 3 |
| `rustls_server_peer_endpoint` + `socket_addr_endpoint`, reading the accepted stream's `raw` handle | §3b, 1 |
| `SSLServerSocket.accept` records both against the session it stashes | §3, 4 |
| the accept path's session and its protocol string are pinned across the two `create_string` calls that follow them | §4, 0 |

**Six new tests**, in the existing `#[cfg(test)] mod tests`:

* `a_recorded_peer_endpoint_answers_the_shape_that_has_no_slot_for_one` — the
  width-4 shape with the ATTRIBUTE MAP populated, i.e. the E31-1 §2 trap armed.
  Asserts the before-state (`null`/`-1`, so the test cannot pass by answering
  the endpoint for everything), then the recorded answer, then that slot 3 is
  still the map. That last assertion is the one that makes this a
  side-table test rather than a widening test.
* `a_recorded_endpoint_does_not_shadow_the_slots_that_carry_one` — the ORDER,
  from the other side: at widths 6 and 8 a *conflicting* recorded row must
  lose. A reader that consulted the table first passes every other test here.
* `an_empty_endpoint_is_not_recorded_at_all` — "nothing to say" stays ABSENT,
  because both readers fall THROUGH an absent row to the socket registry and a
  blank row would shadow the only answer the `createSocket` client shape has.
  Its second half asserts a port-without-host row IS kept.
* `the_local_chain_writer_records_a_chain_and_declines_an_empty_one` — both
  halves, because the empty half is a measured GREEN row
  (`client.localCertificates = null`) and not a degenerate case.
* `an_ipv4_mapped_peer_address_is_reported_in_ipv4_spelling` — `::ffff:127.0.0.1`
  → `127.0.0.1`, plain v4 unchanged, and a genuine IPv6 peer keeping `::1`.
* the existing `peer_host_and_port_do_not_read_the_attribute_slot` and
  `peer_host_and_port_are_read_on_the_shapes_that_carry_them` are left standing
  and still pass by construction: neither records an endpoint, so both exercise
  the fall-through.

**None of the six was run inside the crate.** The logic behind the first five —
the recording contract, the two lookup orders, the fall-through, and the
IPv4-mapped normalisation — was extracted verbatim into
`scratchpad/rs/endpoint.rs` and run under `rustc --edition 2021 --test`: six
tests, all passing.

`rustfmt --edition 2021 --check` was run **in place** and against
`git show HEAD:native-builtins/src/t27_tls.rs`: **63 deviating hunks before and
63 after**, and a body-level diff of the two rustfmt outputs (headers stripped)
is EMPTY — not merely the same count, the same hunks. One hunk this lane did
introduce (a chained `?` expression rustfmt wanted broken over seven lines) was
found that way and rewritten. Zero CR bytes in the file.

---

## 8. What this lane could not settle

* **Any "after" measurement.** No build, and `target-rel4` never appeared. Every
  after is PREDICTED. The four `server.*` predictions rest on §3a (the object is
  reachable, measured on both VMs) and §3d (the chain is the one already proved
  by the handshake succeeding), which is as far as reasoning can carry them.
* **The four client rows.** §2 — the reader is built and the writers are one
  line each in two files this lane does not own. N1.
* **The four `drain.conn.*` rows.** G44-1 §3; not re-investigated.
* **Whether the `SSLEngine` session's post-handshake endpoint diverges.** §1
  measures HotSpot; nothing in the suite measures CratonVM, because no vector
  drives an engine to a completed handshake. N4.
* **Whether recording a server session's local chain regresses a corpus app.**
  It cannot regress one *relative to HotSpot* — `localCerts=1,
  localPrincipal=CN=localhost` is measured on the oracle for this exact
  handshake — but an app that branched on `getLocalCertificates() == null` to
  mean "client mode" now takes the other branch on an accepted socket. That is
  the branch HotSpot takes; unmeasurable beyond that without a build.
