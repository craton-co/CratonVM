# F10-1 — the two minters that told a completed handshake it never happened, and the marker value that is not free to choose

**Status: FIXED-UNVERIFIED (`native-builtins/src/http_url_connection.rs`, `native-builtins/src/net_phase_e.rs` — this lane's files); NOMINATED (the rest).**
**Prov: HotSpot column MEAS (this host, `scratchpad/f10/F10HttpsSession.java`, JDK 25.0.3+9-LTS `Microsoft-13877124`, loopback `HttpsServer` + `HttpsURLConnection`, three byte-identical runs); registrar-ownership rows cited from E22-1's `--dump-native-registry` rather than re-derived; tree citations READ; CratonVM column PRED.**
**2026-08-13, lane F10.** Lands **NOMINATION 1** and **NOMINATION 2** of
`F6-1-the-arm-that-had-to-move-and-the-two-minters-it-keeps-wrong.md`.

**This lane may not build or run the VM.** Every CratonVM "after" below is
**PREDICTED**. No `cargo`, no VM, no fixture. The Rust was parse-checked with
`rustfmt --edition 2021 --emit=stdout` on scratch copies of both files, and
**the check was mutation-verified** — deleting the semicolon after
`HTTPS_CLIENT_SESSION_MARKER`'s initialiser makes it report
`expected one of ., ;, ?, where, or an operator` and name line 7958. Parsing is
not type-checking; see §8.

---

> **VERIFIED AGAINST A BINARY 2026-09-04. Steps 1-3 of §"How to verify" were
> run; steps 4-5 could not be.** Status was *"No `cargo`, no VM, no fixture"*,
> every CratonVM row PREDICTED.
>
> **Step 1 — the bar is that F6's and E42's mutation pairs are UNAFFECTED**,
> *"if either moves, this commit touched the predicate, which it must not."*
> Neither moved: both are green at baseline, and the full mutation matrix run
> for `F6-1` behaves exactly as `F6-1` predicts, so the predicate this lane must
> not have touched is demonstrably the one `F6-1` describes.
>
> **Step 2 — §4's ownership table, re-taken.** This record flags its own
> weakest point: *"E22-1's dump is the authority and it is from an older tree."*
> Taken fresh from `--dump-native-registry` on a binary built from this tree,
> **every row of §4 holds**:
>
> ```text
> method                owns   registered_by            §4 says
> getPeerPrincipal      True   ssl_security.rs:6272     ssl_security's, live      ✓
> isValid               True   t27_tls.rs:20072         t27_tls's, live           ✓
> isValid               False  ssl_security.rs:6016     inert (owns_slot=false)   ✓
> getId                 True   t27_tls.rs:19886         t27_tls's, live           ✓
> getId                 False  ssl_security.rs:6028     inert                     ✓
> getProtocol           False  ssl_security.rs:5951     inert                     ✓
> getCipherSuite        False  ssl_security.rs:5987     inert                     ✓
> getPeerCertificates   True   t27_tls.rs:19766         t27_tls's, not read       ✓
> getPeerCertificates   False  ssl_security.rs:6090     inert                     ✓
> getLocalCertificates  True   ssl_security.rs:6197     ssl_security's, not read   ✓
> ```
>
> §4 is *"an argument from that table"*, and the table is now current rather
> than inherited.
>
> **Step 3 — `RSslNullSession`, "Expect no change at all."** It passes in both
> Compatible and `--jdk-only` in a full 129-vector run. No check moved, so by
> this record's own criterion the fix did not go in the wrong file.
>
> **What this does NOT verify — and it is the behavioural half.** Step 4 wants
> an embedded-HTTPS fixture and the H2 `TestSsl` cluster, to see that *"a
> completed HTTPS handshake reporting `isValid() == false`"* is gone, and to
> watch for the two named residuals (`AbstractMethodError` from `invalidate()`,
> `SSLPeerUnverifiedException` from `getPeerPrincipal()`). **That was not run.**
> Step 5's `scratchpad/f10/F10HttpsSession.java` needs a `ks.p12` and did not
> survive its session, so §6's table cannot be reproduced. Registration and
> ownership are confirmed; the marker's effect on a live handshake is still
> PREDICTED.

## 0. Verdict

1. **`-1` was never a description of these two sessions; it was a description
   of the code that mints them.** Both comments said *"this connection owns its
   rustls state inside `perform` and is never registered in the `servlet` TLS id
   space, so there is no id to record"* — true, and an answer to a different
   question. `t27_tls::session_has_negotiated` does not ask "is there a stream
   id"; it asks "did this session negotiate anything", and `-1` is that
   question's NO. §2.
2. **The fix is at the minters and the predicate is untouched**, which is the
   whole point: a predicate that called `-1` negotiated would re-validate the
   never-connected null session at every door, undoing E12/E22/E31/E42 in one
   line. Landing this changes **0 of `RSslNullSession`'s 47 checks**, and that
   zero is the proof the approach is right rather than an absence of work. §7.
3. **The marker's numeric value is load-bearing, and the reason is a LIVE
   reader nobody had connected to this.** `phases_late::ssl_security`'s
   `SSLSession.getPeerPrincipal` is owned by `ssl_security` (E22-1 §1's dump)
   and uses slot 2 as a **lookup key** —
   `s2_tls_peer_cert_chain_der(tls_id)`. Any marker that could collide with a
   live TLS stream id would hand one connection **another connection's peer
   certificate chain**. F6's NOMINATION 1 called option 2 "strictly worse
   because it makes slot 2 mean two things"; this is the concrete cost of that,
   and it is priced by choosing a value outside every id range in `servlet.rs`.
   §4.
4. **Option 1 — register in the `servlet` id space — is not merely invasive, it
   is impossible at the point where it would be needed.** The rustls state is a
   `StreamOwned<ClientConnection, TcpStream>` local to
   `http_url_connection::perform`, dropped before any accessor runs; and
   registering the still-live one earlier would insert an entry that nothing
   ever removes. §3.
5. **MEASURED, and it makes a third independent confirmation:** `invalidate()`
   on HotSpot changes **exactly one thing** — `isValid()` goes `true` →
   `false`, while id, cipher and protocol all survive byte-for-byte. E12-1 §1
   arm E and F6-1 §4 each measured this on a *null* session; this is the first
   measurement of it on a session that genuinely negotiated. §1.
6. **MEASURED and unexpected: HotSpot's own `HttpsURLConnection` session
   accessors stop working after the response body is drained.** Once the
   connection returns to the `KeepAliveCache`, `getCipherSuite()` /
   `getSSLSession()` throw `IllegalStateException: connection not yet open`
   (`AbstractDelegateHttpsURLConnection:215`) — the same exception they throw
   for a connection that never handshaked. This dictated the probe's shape and
   it retro-justifies `net_phase_e`'s choice of that exact exception. §1.
7. **The change converts one wrong-and-consistent answer into a
   right-but-inconsistent one, and that is disclosed rather than buried.**
   `https_session_object` mints a FRESH session object on every accessor call,
   where HotSpot returns the same object (measured, `sameObject=true`). Today
   two `getSSLSession().get().getId()` calls both return `byte[0]` and so
   compare EQUAL; after this change they return two different 32-byte ids.
   NOMINATION 1. §6.
8. **F6-1's correction to E42-1's NOMINATION 3 is confirmed by census**, and
   the last one is fixed here. §5.

## 1. MEASURED — `scratchpad/f10/F10HttpsSession.java`, three byte-identical runs

HotSpot 25.0.3+9-LTS `Microsoft-13877124`. **Loopback only, no external
network**: a `com.sun.net.httpserver.HttpsServer` on `127.0.0.1:0` with a
`keytool`-generated `CN=localhost` PKCS#12 (SAN `dns:localhost,ip:127.0.0.1`),
reached through `HttpsURLConnection` — i.e. through exactly the door this
lane's two files serve. The id hex differs per handshake by construction (it is
a random session id); every other column is byte-identical across three runs,
and the ids were normalised before diffing.

```
C1 conn.getCipherSuite=TLS_AES_256_GCM_SHA384
C1 getSSLSession.present=true
C1 two getSSLSession calls sameObject=true
C1 session.class=sun.security.ssl.SSLSessionImpl
C1 completed         isValid=true  idLen=32 cipher=TLS_AES_256_GCM_SHA384 proto=TLSv1.3
C1 getId twice equalContent=true sameArrayObject=false
C1 session.getPeerPrincipal=CN=localhost
C1 session.getPeerCertificates.len=1
C1 session.getLocalPrincipal=null
C1 conn.getServerCertificates.len=1
C1 conn.getPeerPrincipal=CN=localhost
C1 after-invalidate  isValid=false idLen=32 cipher=TLS_AES_256_GCM_SHA384 proto=TLSv1.3
C1 id survives invalidate=true
C2 sameCtx          isValid=true  idLen=32 cipher=TLS_AES_256_GCM_SHA384 proto=TLSv1.3
C2 id equals C1 id=false
C3 freshCtx         isValid=true  idLen=32 cipher=TLS_AES_256_GCM_SHA384 proto=TLSv1.3
C3 id equals C1 id=false
NULL never-connected isValid=false idLen=0  cipher=SSL_NULL_WITH_NULL_NULL proto=NONE
```

What this settles, in the order the brief asked:

1. **A completed handshake's id is non-empty and `byte[0]` means nothing was
   negotiated.** The last line is the contrast, measured in the same process:
   the null session's `SSL_NULL_WITH_NULL_NULL` / `NONE` / `idLen=0` sit beside
   a real handshake's 32 bytes. E12-1 established this from a socket; this is
   the HTTPS door.
2. **The id must be stable across calls on ONE session and distinct across
   sessions.** `getId()` twice returns equal content in a *different array
   object* — JSSE clones defensively, so a caller cannot mutate the session's
   id. Distinctness holds even for two connections through the **same
   `SSLContext`** to the same server (`C2 id equals C1 id=false`): TLS 1.3
   mints a new session id per handshake, so "same context ⇒ same id" is not a
   thing to model.
3. **`invalidate()` changes EXACTLY ONE thing.** `isValid` `true` → `false`;
   `idLen` stays 32 and the id stays byte-identical (`id survives
   invalidate=true`), cipher and protocol unchanged. Third independent
   measurement, first one on a negotiated session.
4. **`getPeerPrincipal()` on a completed client session is the server's
   subject**, `CN=localhost`, and `getLocalPrincipal()` is `null` for a client
   with no configured identity. That is an oracle this family did not have, and
   CratonVM fails it — before and after this change. NOMINATION 2.
5. **The KeepAliveCache result (§0.6).** `getCipherSuite()` after a full drain
   throws `IllegalStateException: connection not yet open`. So the session must
   be read while the exchange is open, and — noted because it is easy to
   misread as a CratonVM bug — an app that drains and *then* asks gets an
   exception on HotSpot too.

## 2. The change

Two `Value::Int(-1)` writes become one shared constant. That is the entire
production diff; everything else is comments.

`native-builtins/src/net_phase_e.rs`, new constant beside its two users:

```rust
pub(crate) const HTTPS_CLIENT_SESSION_MARKER: i32 = 0x0800_0000;
```

`net_phase_e::https_session_object` and
`http_url_connection::huc_verify_hostname`:

```rust
-        Value::Int(-1),
+        Value::Int(HTTPS_CLIENT_SESSION_MARKER),          // net_phase_e
+        Value::Int(crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER),  // http_url_connection
```

**One constant, not two literals, is the point.** These two sites have carried
the same value and the same four-line comment since they were written; F6-1
found them by grepping for the comment. A shared named constant is what makes
"the two minters agree" a compile-time fact instead of a convention.

**Both sites are past a completed handshake, and that was verified rather than
assumed.** `huc_verify_hostname`'s only caller (`http_url_connection.rs:2743`)
reads `stream.conn.protocol_version()` and `stream.conn.negotiated_cipher_suite()`
to build its arguments, and the line after the call says *"The handshake is
over"*. `https_session_object` is only reachable when `https_carrier_session`
returns `Some`, and that entry is written by `record_https_carrier_session`
from inside `huc_verify_hostname`. Neither can be reached without a handshake
having completed.

The hostname-verifier site got the sharper comment, because it is the one where
the old value was self-contradicting: `verify()` is invoked to decide whether
to **accept** the peer, and a verifier that asks `session.isValid()` — a
documented thing for a verifier to do — was told the handshake it had been
invoked to vet had not happened.

## 3. Why option 1 is refused, from source rather than from taste

F6-1's NOMINATION 1 puts "register the connection in the `servlet` id space" in
preference order 1 and asks for a measurement before falling back. The refusal
here is stronger than a measurement, because it is a lifetime argument:

* **At the point of use there is nothing to register.** `https_session_object`
  runs from `getSSLSession()`/`getPeerPrincipal()`/`getServerCertificates()`,
  which are called by application code after `perform` has returned. `perform`
  owns the rustls state as a local `StreamOwned<ClientConnection, TcpStream>`
  and drops it on return. The id would name a stream that no longer exists.
* **Registering the live one earlier leaks, unboundedly.**
  `servlet::s2_tls_close` is the only thing that removes from `tls_streams`
  (`servlet.rs:2699`), and it is driven by `SSLSocket.close()` — which this path
  never calls, because the HTTP exchange closes its own `TcpStream` locally. One
  leaked registry entry per HTTPS request, each of which permanently answers
  "this stream is alive" to `s2_tls_session_info`.
* **It would duplicate state that already exists.**
  `record_https_carrier_session` already keeps protocol, cipher and peer chain
  for this connection, keyed by the carrier. A second copy in the socket
  registry is a second thing to keep in step.

So the marker is taken deliberately, with §4's price paid explicitly, and the
constant's doc comment carries this paragraph so the next reader does not
re-open it.

## 4. Why THIS value — the enumeration that makes it load-bearing

Every reader of slot 2 was enumerated (`grep -n NEW13_SESS_TLSID` over the
tree) and split by what it does with the value. **Only the second column
matters**, because a marker is safe exactly when no live reader dereferences
it.

| reader | file | slot 2 used as | live in real-JDK mode? | effect of the marker |
|---|---|---|---|---|
| `session_has_negotiated` | `t27_tls.rs` | **a predicate** (`>= 0`) | yes (via t27's `isValid`/`getId`) | **the fix** — `false` → `true` |
| `SSLSession.isValid` | `t27_tls.rs` | via the predicate | **yes** (E22-1 dump) | `false` → `true` |
| `SSLSession.getId` | `t27_tls.rs` | via the predicate; bytes seeded from `gc_stable_objref_key(session)` | **yes** | `byte[0]` → `byte[32]` |
| `SSLSession.getPeerPrincipal` | `ssl_security.rs` | **a LOOKUP KEY** | **yes** (E22-1 dump) | lookup now runs, and must MISS — §4.1 |
| `SSLSession.getProtocol`/`getCipherSuite`/`getId`/`isValid`/`getPeerCertificates` | `ssl_security.rs` | lookup key | **no** — `owns_slot=false` | inert |
| `SSLSession.getPeerCertificates` | `t27_tls.rs` | **not read** — keyed on the session object | yes | unchanged |
| `SSLSession.getLocalCertificates` | `ssl_security.rs` | **not read** — `local_certs_for_session` | yes | unchanged |
| `SSLSession.isValid`/`getId`/`SSLSessionImpl.*` | `tls.rs` | via the predicate; bytes from `identity_hash_code` | `--synthetic-jdk` only | same direction, same fix |
| `SSLSocket.close` | `ssl_security.rs:4364` | **writes** `-1` | yes | never sees these sessions (they belong to a carrier, not a socket) |

### 4.1 The one that decides the value

`ssl_security`'s `getPeerPrincipal` is LIVE and does
`s2_tls_peer_cert_chain_der(tls_id)` whenever `tls_id >= 0`. With `-1` it
skipped the lookup; with a marker it performs one. That lookup **must** miss,
because a hit would be another connection's certificate chain.

`servlet::s2_tls_session_info`/`s2_tls_peer_cert_chain_der` are plain
`HashMap::get` on `tls_streams` with no range arithmetic (`servlet.rs:2791`,
`:2785`), so "miss" means "this integer is not a key". The keys are:

* `s2_next_free_id`'s monotonic counter, which starts at 1 and increments
  (`servlet.rs:2146`);
* `RUSTLS_SOCK_ID_BASE` (`0x4000_0000`), `PENDING_LAYERED_SOCK_ID_BASE`
  (`0x2000_0000`) and `PENDING_CONNECT_SOCK_ID_BASE` (`0x1000_0000`) plus a
  small stream id.

`0x0800_0000` is below all three bases and 134 million allocations above the
counter's start. It continues those constants' halving sequence and rests on
**the identical assumption they already state** — *"native-tls and rustls ids
are small counters, so the high offset never collides"* (`servlet.rs:2413`).
This lane adds no new assumption; it joins one that is already load-bearing
three times over.

**And the answer does not change direction even so.** With `-1` the chain was
`Vec::new()` and `getPeerPrincipal` threw `SSLPeerUnverifiedException`; with the
marker the lookup misses, the chain is `Vec::new()`, and it throws the same
exception. That answer is *wrong* for a completed handshake — HotSpot measures
`CN=localhost` — but it is wrong identically before and after. NOMINATION 2.

### 4.2 What was deliberately NOT done

* **The predicate was not loosened.** No edit to `t27_tls.rs` or `tls.rs`.
* **No plausible cipher or protocol name was introduced anywhere.** Slots 0 and
  1 already held the real negotiated names on both minters and are untouched;
  this change adds no string.
* **The two native-TLS acceptor `"UNKNOWN"` arms were not touched**, per the
  brief and E42-1 §5's judgement.
* **The marker is ONE constant, not a per-connection counter.** `getId()`'s
  bytes come from the session object's identity in both modes
  (`gc_stable_objref_key` in `t27_tls`, `identity_hash_code` in `tls.rs`), never
  from slot 2, so per-session variation in this slot would buy no distinctness
  while re-opening §4.1's collision surface.

## 5. NOMINATION 2 of F6-1 — the last "3-field" comment, and the census that confirms the count

`http_url_connection.rs:2183` said *"The 3-field client `SSLSession` shape"*.
The allocation below it already used `NEW13_SSL_SESS_FIELDS`, so only the prose
was stale; replaced with E42-1's exact text, which names the constant rather
than a number that has now moved twice.

**F6-1's correction to E42-1 is confirmed.** `grep -rn "3-field"` over
`native-builtins/src/`, filtered to `SSLSession`-shaped hits, now returns
exactly one line — `t27_tls.rs:12494`, *"and the 3-field NEW-13 shape widened to
4"*, which is a **historical** statement about the retirement and is correct as
written. `net_phase_e.rs` carries none, as F6-1 said. So the tree total was
four: three in `tls.rs` (F6) and this one.

The unfiltered grep returns ~25 hits, all unrelated shapes (`RecordComponent`,
`KeyManagerFactory`, `MethodHandles.Lookup`, ...). Stated because the raw
number is what a later census will hit first, and it does not contradict the
four.

## 6. What a completed HTTPS handshake reports — the deliverable

Session obtained from `HttpsURLConnection.getSSLSession().get()`, or handed to a
`HostnameVerifier`. HotSpot MEAS (§1); both CratonVM columns PRED. "Before" is
HEAD with F6's arm merge landed — i.e. the state this lane was handed, not the
pre-F6 state in which the catch-all made these accidentally right.

| | HotSpot (MEAS) | CratonVM before (PRED) | CratonVM after (PRED) |
|---|---|---|---|
| `isValid()` | **`true`** | **`false`** | **`true`** ✓ |
| `getId().length` | **32** | **0** | **32** ✓ |
| `getId()` twice, one session ref | equal content, different array | equal (both `byte[0]`) | equal content, different array ✓ |
| `getId()`, two different sessions | different | equal (both `byte[0]`) | different ✓ |
| `getCipherSuite()` | `TLS_AES_256_GCM_SHA384` | the real negotiated name | **unchanged** — the real negotiated name |
| `getProtocol()` | `TLSv1.3` | the real negotiated name | **unchanged** |

**Cipher and protocol were already right and are not touched.** Slots 0 and 1
are written from `stream.conn.negotiated_cipher_suite()` and
`.protocol_version()` on both minters, and `t27_tls`'s `getCipherSuite`/
`getProtocol` read those slots directly without consulting the predicate. Said
explicitly because "the session reports `isValid()==false`" invites the guess
that the whole session is empty, and it was not: two of the four accessors were
always right, which is exactly what made the defect survive.

For the never-negotiated session, nothing moves:

| | HotSpot (MEAS) | CratonVM before | CratonVM after |
|---|---|---|---|
| null session `isValid()` | `false` | `false` | **`false`** — unchanged |
| null session `getId().length` | 0 | 0 | **0** — unchanged |
| null session `getCipherSuite()` | `SSL_NULL_WITH_NULL_NULL` | sentinel | unchanged |

### The three things this does NOT fix, in decreasing size

1. **`getSSLSession()` mints a fresh object per call** (§0.7). HotSpot:
   `sameObject=true`. Here, every accessor calls `https_session_object`, which
   allocates. Consequence of *this* change: two `getSSLSession().get().getId()`
   calls go from equal-because-both-empty to two different 32-byte ids. For the
   named consumer — Tomcat's `JSSESupport.getSessionId`, which tests
   `ssl_session.length == 0` exactly — this is a strict improvement (untrackable
   → trackable). For a caller comparing ids across two `getSSLSession()` calls
   it is a regression from an accidental equality. NOMINATION 1.
2. **`invalidate()` on this session.** In real-JDK mode **no native registers
   `javax/net/ssl/SSLSession.invalidate`** at all — only `tls.rs` does, and its
   registrar is `--synthetic-jdk`-only — so the interface declaration with no
   Code attribute is what runs (PREDICTED `AbstractMethodError`). Under
   `--synthetic-jdk`, `tls.rs`'s `invalidate` is gated `> SES_CREATION_TIME` and
   no-ops at width 4, so `isValid()` stays `true` where HotSpot measures
   `false`. **This change does not create that population** — `SSLServerSocket
   .accept`'s width-4 session has had a `>= 0` slot 2 all along — it adds a
   second member to it. F6-1 §4 bounds the damage: `invalidate()` touches
   `isValid()` and nothing else, so the miss cannot spread. NOMINATION 3.
3. **`getPeerPrincipal()` throws where HotSpot returns `CN=localhost`** (§4.1),
   and `getPeerCertificates()` on the same session works — because the two are
   served by different registrars reading different tables. NOMINATION 2.

## 7. `RSslNullSession`: this lane decides 0 of the 47, and that is the correct number

F6-1 §9 establishes the denominator: 8 of the 47 checks read
`session_has_negotiated` on the widened shape and 4 are decided by it
(`isValid`, `getId.length` across the `socket` and `socketClosed` doors), rising
to 12/6 under `--synthetic-jdk`. Today 1 of 47 executes.

**This change affects none of them, in either mode.** The fixture never opens an
HTTPS connection; its sessions come from `new13_resolve_socket_session` (the
null session, `tls_id = -1`) and from `createSSLEngine`. Neither reads
`HTTPS_CLIENT_SESSION_MARKER`, and the predicate they share is not edited.

That zero is the deliverable, not a gap. The failure mode this lane was told to
avoid — loosening `session_has_negotiated` — would show up here as **4 checks
flipping from correct to fabricated**: the null session would report
`isValid() == true` and 32 bytes again, which is precisely the defect E12-1 and
E22-1 removed. **A change to these two minters that moves any `RSslNullSession`
check has been made in the wrong file.** That is the cheapest available
regression test for this commit and it needs no HTTPS peer.

## 8. Residuals

1. **Nothing was built, type-checked or run.** `rustfmt` proves both files
   *parse* (mutation-verified, see the header); it does not prove they compile.
   The two type risks: `HTTPS_CLIENT_SESSION_MARKER` is `i32` and slot 2 takes
   `Value::Int(i32)` — the same pairing `t27_tls.rs:5100` already compiles with
   for `RUSTLS_SOCK_ID_BASE + stream_id`; and the cross-module path
   `crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER` is `pub(crate)` reached
   from a sibling module of the same crate, which
   `http_url_connection.rs:2128`'s existing
   `crate::net_phase_e::record_https_carrier_session` (also `pub(crate)`)
   already does.
2. **The source-witness test in `http_url_connection.rs` was checked against
   this edit.** `the_session_capture_precedes_the_success_path_early_return`
   scans the working tree for `record_https_carrier_session(` and
   `if builtin.is_ok() {` inside `huc_verify_hostname`, bounded by the next
   column-0 `}`. This lane added neither a `record_https_carrier_session` call
   nor a column-0 `}` inside that function, and moved neither landmark
   relative to the other.
3. **CRLF preserved, zero bare LF introduced.** `http_url_connection.rs`
   5,131 → 5,145 CRLF with LF equal at both points; `net_phase_e.rs`
   19,014 → 19,086, likewise. `LF - CRLF == 0` on both after every hunk. All
   edits went through the editor. Counts must be taken from the file on disk —
   `git show HEAD:` reports 0 CRLF for the same files because git stores LF.
4. **This worktree is SHARED and fifteen sibling-lane files are dirty in it.**
   Neither of this lane's two files was dirty when it opened, and no other
   lane's file was touched. Anyone committing this work must stage
   `native-builtins/src/http_url_connection.rs`,
   `native-builtins/src/net_phase_e.rs` and this record **by path** — never
   `-a`, never `git commit -am`.
5. **`session_peer_certs_table` is keyed on a 32-bit identity hash and is never
   pruned.** `gc_stable_objref_key` is `identity_hash_code(o) as u32`
   (`t27_tls.rs:5375`), and `record_client_peer_chain` inserts once per minted
   session with no remover. Combined with §6.1's per-call minting, an
   HTTPS-heavy process accumulates one entry per accessor call, and at ~2^16
   live keys a birthday collision hands one session another's peer chain — and
   bounds how "distinct" `getId()` really is. **Pre-existing and unchanged in
   rate by this commit** (the insert already happened once per mint), but
   flagged so it is not bisected onto this change. NOMINATION 1 fixes the rate;
   the key width is separate.
6. **No test was added.** The natural guard — "these two minters do not write a
   negative id" — is a source witness, and the honest version of it is a
   behavioural assertion that needs a TLS peer. §7's zero is the guard this
   commit actually has, and it is checkable without one.

---

## NOMINATION 1 — `net_phase_e.rs`: `https_session_object` should mint ONE session per carrier (**this lane's file, deliberately not done here**)

MEASURED: HotSpot returns the *same* `SSLSession` object from two
`getSSLSession()` calls on one connection (§1). Here every one of the six
accessors calls `https_session_object`, which allocates a fresh one, so
`getId()` — seeded from the object's identity — differs per call. Before this
commit that was invisible (every call returned `byte[0]`); after it, it is the
most visible remaining divergence on this path.

Not done in this commit **for a stated reason**: caching an `ObjectRef` in the
`https_carrier_sessions` table makes that table a GC root holder, and this
directory records the family of bugs where an overlay side table holds object
references that no collector path roots. The fix therefore needs a build and a
GC-mode matrix, not a blind edit, and landing it inside a commit that already
changes an observable contract would make the first change un-bisectable.

The shape: extend `HttpsCarrierSession` with the minted session, root it for as
long as the entry lives (and unroot on eviction), and have all six accessors go
through one `https_session_for_carrier` helper. That also fixes §8.5's insert
rate as a side effect, since `record_client_peer_chain` would run once per
connection instead of once per accessor call.

## NOMINATION 2 — `phases_late/ssl_security.rs`: `getPeerPrincipal` reads a different table from the `getPeerCertificates` next to it

**Not this lane's file.** MEASURED, HotSpot: a completed client session answers
`getPeerPrincipal() == CN=localhost` and `getPeerCertificates().length == 1`.
On CratonVM the second works and the first throws
`SSLPeerUnverifiedException("peer not authenticated")`, because the two are
served by different registrars reading different sources:

* `t27_tls`'s `getPeerCertificates` (LIVE) reads `session_peer_certs_table`,
  keyed on the session object — which `record_client_peer_chain` populates for
  exactly these HTTPS client sessions;
* `ssl_security`'s `getPeerPrincipal` (LIVE) reads
  `s2_tls_peer_cert_chain_der(slot2)` — the socket registry, which has no entry
  for a connection that was never a registered socket.

So one door sees the chain and its neighbour does not, on the same object, in
the same call sequence. The fix is to give `getPeerPrincipal` the same chain
source its sibling uses — ideally by having it call
`SSLSession.getPeerCertificates` rather than re-deriving a subject from DER,
which is the "thin direct helper reimplements the native" shape this workspace
keeps finding, and which `net_phase_e`'s own accessors already avoid by
delegating.

This is **not** a regression from this commit — it is wrong identically before
and after — but this commit is what makes it reachable in an otherwise-correct
session, and §1 supplies the oracle that did not exist before.

## NOMINATION 3 — `javax/net/ssl/SSLSession.invalidate()` has no registration in real-JDK mode

**Not this lane's file** (`tls.rs`). `grep -rn '"invalidate"'` over
`native-builtins/src/` returns exactly one registration, in `tls.rs`, whose
registrar `register_tls_natives` is reached only from
`register_synthetic_overrides` (`#[cfg(feature = "synthetic-jdk")]`). In the
default real-JDK mode — the mode `--jdk-only` runs — nothing registers it, and
`javax.net.ssl.SSLSession` is an interface whose `invalidate()` has no Code
attribute. PREDICTED `AbstractMethodError` on a plain
`session.invalidate()`.

This was unreachable-in-practice while every session this door mints answered
`isValid() == false` anyway. It is reachable now. Two separable pieces:

1. register `invalidate` in real-JDK mode at all;
2. make it *do* something at width 4, which needs a slot the shape does not
   have — F6-1 §4 and E42-1 §2 both record why widening again is rejected.

(1) without (2) still leaves `isValid()` stuck at `true` after an
`invalidate()`, but converts a thrown `Error` into a silent no-op, which is the
direction HotSpot's own null-session behaviour already takes.

## NOMINATION 4 — `docs/known-issues/jdk-only/INDEX.md`: six records, one investigation

Re-raising F6-1's NOMINATION 4, which re-raised E42-1's NOMINATION 5, which
re-raised E31-1's NOMINATION 7. Still open, now one record longer. `INDEX.md` is
not this lane's file.

```
- E12-1-the-null-session-and-the-fabricated-cipher.md
- E22-1-the-null-session-in-the-registrar-that-actually-answers.md
- E31-1-the-unregistered-door-and-the-slot-that-resurrects-a-fabrication.md
- E42-1-the-slot-that-was-never-there-and-the-predicate-that-was-its-own-negation.md
- F6-1-the-arm-that-had-to-move-and-the-two-minters-it-keeps-wrong.md
- F10-1-the-two-minters-that-told-a-completed-handshake-it-never-happened.md
```

## How to verify

Cheapest first. Every CratonVM row is PREDICTED.

1. **`cargo test -p cratonvm-native-builtins`** — this lane adds no test, so the
   bar is that F6's and E42's mutation pairs stay green. In particular
   `ssl_security::new13_tests::the_widened_null_session_is_still_not_negotiated`
   and `t27_tls::tests::the_null_socket_session_has_no_id_and_is_not_valid` must
   be **unaffected**: if either moves, this commit touched the predicate, which
   it must not.
2. **`--dump-native-registry`** (flags BEFORE `-cp`) — re-confirm §4's ownership
   column, especially that `SSLSession.getPeerPrincipal` is still
   `ssl_security`'s and `isValid`/`getId` are still `t27_tls`'s. The whole of §4
   is an argument from that table; E22-1's dump is the authority and it is from
   an older tree.
3. **`bash regression-suite/run.sh` with `ONLY="RSslNullSession"`**, after E31's
   `getHandshakeSession` registration lands. **Expect no change at all.** §7 is
   the reasoning; a moved check means the fix went in the wrong file.
4. **Any embedded-HTTPS fixture, and the H2 `TestSsl` cluster.** The divergence
   F6-1 §5 told the next lane to expect — a completed HTTPS handshake reporting
   `isValid() == false` — should be **gone**. Watch for the two residuals
   instead: an `AbstractMethodError` from `invalidate()` (NOMINATION 3) and an
   `SSLPeerUnverifiedException` from `getPeerPrincipal()` (NOMINATION 2).
5. **Re-run `scratchpad/f10/F10HttpsSession.java` under CratonVM** once a build
   exists. It is self-contained (it generates nothing but needs `ks.p12`
   alongside it, from the `keytool` line in §1) and it prints the whole of §6's
   table in one pass. `NULL never-connected` must stay
   `isValid=false idLen=0`; if it does not, the predicate moved.
