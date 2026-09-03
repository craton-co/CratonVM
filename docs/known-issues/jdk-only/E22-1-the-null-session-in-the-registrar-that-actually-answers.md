# E22-1 — the null session in the registrar that actually answers, and the row the nomination missed

**Status: FIXED-UNVERIFIED (`native-builtins/src/t27_tls.rs`, this lane's file); NOMINATED (the rest).**
**Prov: HotSpot column MEAS (E12-1's transcripts, re-checked here against `C:\craton\jdk25src`); CratonVM column PRED.**
**2026-08-13, lane E22.** Lands NOMINATIONS 1–7 of
`E12-1-the-null-session-and-the-fabricated-cipher.md`.

> **VERIFIED AGAINST A BINARY 2026-09-02.** "This lane may not build or run the
> VM. Every CratonVM 'after' below is PRED." The fixture has now been run on a
> build of this tree, against HotSpot on the same host:
>
> ```text
> HotSpot              rc=0   91 CK lines
> CratonVM compatible  rc=0   91 CK lines   diff vs HotSpot: EMPTY
> CratonVM --jdk-only  rc=0   91 CK lines   diff vs HotSpot: EMPTY
> ```
>
> **The CK LINES were counted, not the vector's verdict**, and that distinction
> is this fixture's own history. `E31-1` §"unblocked" records that
> `RSslNullSession` "runs **1 of its 47 checks** today — it dies on" the door it
> names. A vector that dies after one check can still exit 0 and be reported by
> the suite as `1 passed`; through `regression-suite/run.sh` that is exactly what
> this looks like. Counting `^CK` lines is what tells the two apart, and all 91
> are present on both arms.
>
> **Ninety-one, not the forty-seven §6 specifies.** The fixture was extended
> after this record was written. That is ordinary growth, unlike `W7-5`'s count,
> and the assertions are what was checked: the diff against HotSpot is empty
> line-for-line, so every check §6 specifies is among them and agrees.
>
> `E12-1` §6's three doors are all exercised — the unconnected
> `SSLSocketFactory.getDefault().createSocket()` arm and the same socket after
> `close()`, the `createSSLEngine()` arm before any handshake, and the mechanised
> cipher-suite argument. §6's own falsifier is that
> `TLS_AES_256_GCM_SHA384` / `TLS_AES_128_GCM_SHA256` must read true; the output
> is byte-identical to HotSpot's, so they do.
>
> **Scope.** This closes the "CratonVM column PRED" for the surface the fixture
> covers, on both arms. It does not touch the NOMINATED rows in other files, and
> it is not evidence for any door the fixture does not open.

**This lane may not build or run the VM.** Every CratonVM "after" below is
**PREDICTED**. Nothing here was compiled.

---

## 0. Verdict

E12-1 got the contract right and the reach wrong in one specific place, and
that place is the one that decides an unconnected `SSLSocket`.

1. **The live/dead table is confirmed by the dump, not by source order.**
   `--dump-native-registry` (`scratchpad/p1/reg.json`, `mode: compatible`)
   settles all three of E12-1's dump-dependent predictions: on
   `javax/net/ssl/SSLSession`, `t27_tls.rs` **owns** `getProtocol`,
   `getCipherSuite`, `getId`, `isValid`, `getCreationTime`,
   `getLastAccessedTime` and `getPeerCertificates`; `ssl_security.rs`'s copies
   carry `owns_slot: false`. That lane's accessor fixes were inert. §1.
2. **NOMINATION 3 as written would have been a no-op for DOOR 1.** It gated
   `getId` on `object_num_fields(this) < 7 || slot2 != 0`, i.e. it treated
   *every* shape narrower than 7 fields as negotiated. The unconnected
   `SSLSocket`'s session is the **3-field** shape, so it would have kept its 32
   fabricated bytes. §3.
3. **`isValid` had the same hole and no nomination at all.** t27's `isValid`
   answered `Value::Int(1)` for every shape narrower than 7 fields, so an
   unconnected `SSLSocket.getSession().isValid()` returned `true` where HotSpot
   measures `false`. E12-1's live/dead table lists `isValid` among the accessors
   t27 wins, but its NOMINATIONS never asked for it. §3.
4. **The fixture cannot reach any of this yet, and not for the reason E12-1
   predicted.** `RSslNullSession` DOOR 1's *second* check is
   `s.getHandshakeSession()` on the socket. No native is registered for
   `javax/net/ssl/SSLSocket.getHandshakeSession` — confirmed by the dump — so
   the real JDK body runs, and the real JDK body is
   `throw new UnsupportedOperationException()` (verified in
   `jdk25src/java.base/javax/net/ssl/SSLSocket.java:474`). The vector aborts
   there, before any session assertion. §6, NOMINATION A.

## 1. The live/dead verdict, re-checked against the dump

`scratchpad/p1/reg.json`, parsed with python. `owns_slot` is the authority;
both rows exist in the dump because it records registration *events*.

| method on `javax/net/ssl/SSLSession` | `ssl_security.rs` | `t27_tls.rs` |
|---|---|---|
| `getProtocol` | `owns_slot=false` | **`true`** |
| `getCipherSuite` | `owns_slot=false` | **`true`** |
| `getId` | `owns_slot=false` | **`true`** |
| `isValid` | `owns_slot=false` | **`true`** |
| `getCreationTime` / `getLastAccessedTime` | `owns_slot=false` | **`true`** |
| `getPeerCertificates` | `owns_slot=false` | **`true`** |
| `getPeerPrincipal`, `getLocalCertificates`, `getLocalPrincipal` | **`true`** | not registered |
| `getPacketBufferSize`, `getApplicationBufferSize`, `getValue*`, `putValue` | not registered | **`true`** |

E12-1 §5's table is therefore correct as written. Two things it did not say:

* The dump is from an **older tree** — its `ssl_security.rs` line numbers
  (`4340`, `4360`, `4380`, `4392`) sit ~200 lines below the current ones,
  i.e. it predates E12-1's own edits. That does not weaken it: E12-1 changed
  method *bodies*, not the registration order the ownership derives from.
  Anyone re-running the dump should expect the line numbers to move and the
  `owns_slot` column not to.
* `javax/net/ssl/SSLEngine.getSession` is owned by **`ssl_security.rs`**
  (E12-1's edit 7, live), while `sun/security/ssl/SSLEngineImpl.getSession`
  and `.getHandshakeSession` are owned by **`t27_tls.rs`**. Both doors now
  answer the same sentinels, which is what makes the pair consistent rather
  than merely each-correct.

## 2. What landed, in the order the sequencing constraint requires

All in `native-builtins/src/t27_tls.rs`.

| # | site | before | after (PRED) |
|---|---|---|---|
| 1 | `build_synthetic_ssl_session` protocol arm | `_ => "TLSv1.3"` | `JSSE_NULL_PROTOCOL` |
| 1 | same fn, cipher arm + the whole-`unwrap_or_else` tuple | `TLS_AES_256_GCM_SHA384` | `JSSE_NULL_CIPHER_SUITE` |
| 2 | same fn, slot 2 (`isValid`) | `Value::Int(1)` unconditionally | set iff the connection negotiated a suite |
| 3 | `getId` | 32 fabricated bytes always | `byte[0]` unless `session_has_negotiated` |
| 3b | `isValid` | `Int(1)` for every shape `< 7` fields | the same one predicate |
| 4 | `getProtocol` / `getCipherSuite` | `ctx.get_field(..)` raw — a **null String** when the slot was never written | the sentinel |
| 5 | `getHandshakeSession` | a populated session, unconditionally | non-null **only** while `conn.is_some() && is_handshaking()` |
| 5 | `getCreationTime` / `getLastAccessedTime`, shapes ≤ 5 fields | `Value::Long(0)` | `epoch_millis_now()` |
| 6 | the buffer-size comment | *"KEEP (correct constants…) the real JDK returns 16384"* | the two measured numbers and the JDK's actual derivation |
| 7 | four `_ => "TLS"` arms and four `unwrap_or_else(\|\| "UNKNOWN")` producers, the `rustls_session_info` miss tuple, and the self-test's `"?"` | six raw spellings | the two constants |
| — | `build_synthetic_ssl_session` GC pinning | `ses` unpinned across three `create_string` calls | pinned, re-read |

**The intermediate states the ordering constraint exists to prevent**, spelled
out because a reviewer splitting this commit needs to know which halves are
unsafe alone:

* **2 without 1** → `isValid() == false` while `getCipherSuite()` still answers
  `TLS_AES_256_GCM_SHA384`. A session reporting a strong TLS 1.3 suite while
  denying it is valid is a state no real JSSE session can occupy, and it is
  *worse* than either bug alone: code that checks validity first now skips the
  cipher check and code that checks the cipher first now trusts a session the
  VM has already declared dead.
* **3 without 2** → a pure no-op on the 8-field engine shape (slot 2 is still
  `1`, so `session_has_negotiated` still says yes) — but **not** a no-op on the
  3-field shape, which is why 3 is not merely dependent on 2 here the way
  E12-1 assumed. See §3.
* **3b without 3** → `isValid() == false` beside a 32-byte id, i.e. the
  mirror-image of the state above, and specifically the state Tomcat reads
  worst: `JSSESupport` would still accept the id as a tracking key.

Landing order within the single change is 1 → 2 → 3 → 3b → 4 → 5 → 6 → 7.

## 3. THE CORRECTION — five shapes, and the one the nomination did not cover

`javax/net/ssl/SSLSession` is minted at **five different widths** in this tree,
and the "nothing was negotiated" signal lives somewhere different in each:

| width | minted by | slot 2 holds | negotiated when |
|---|---|---|---|
| 2 | `ssl_security.rs`'s `SSLEngine.getSession` fallback | — | **never** — the shape exists only to carry the sentinel pair |
| 3 | `ssl_security::new13_alloc_null_ssl_session` **and** `new13_alloc_ssl_session` | `NEW13_SESS_TLSID` | a stream id was recorded (`>= 0`); the **null session carries `-1`** |
| 4 | `SSLServerSocket.accept` (`t27_tls.rs`) | the stream id, always `>= 0` | always |
| 6 | `http2.rs`'s `HttpResponse.sslSession()`; `tls.rs` | — | always |
| 8 | `build_synthetic_ssl_session` (`t27_tls.rs`) | the `isValid` flag | the flag is set |

NOMINATION 3's predicate was `object_num_fields(this) < 7 || slot2 != 0`. Row 3
is the one that matters and it is the one that predicate gets wrong: the
unconnected `SSLSocket` session is 3 fields wide, so `< 7` is true, so `getId`
would have returned its 32 fabricated bytes exactly as before. E12-1's own §5
records that `ssl_security`'s `getId` — the one that *does* handle this row —
is **dead** in real-JDK mode. The two halves would have cancelled: a correct
fix in a dead registrar and an incomplete fix in the live one.

The landed predicate is a single function, `session_has_negotiated`, branching
on width. `isValid` and `getId` both call it, and that is not a tidiness
choice — **it is the JDK's own structure**, see §4.

## 4. First-party confirmation, from `jdk25src` rather than re-derivation

E12-1's numbers were measured by running probes. This lane did not re-run them;
it read the oracle's source, which is a genuinely independent check and settles
three questions the measurement could only assert.

* **`getId() == byte[0]` is structural, not incidental.**
  `SSLSessionImpl()` (the no-arg "null session" constructor,
  `sun/security/ssl/SSLSessionImpl.java:157`) sets
  `protocolVersion = ProtocolVersion.NONE`, `cipherSuite = CipherSuite.C_NULL`,
  `sessionId = new SessionId(false, null)`, `host = null`, `port = -1`,
  `creationTime = System.currentTimeMillis()`. And `SessionId(false, null)` is
  `sessionId = new byte[0]` (`SessionId.java:44-50`). Every cell of E12-1's
  arm-A row is in that constructor.
* **`isValid()` and `getId()` ARE one predicate in the JDK.** `isValid()` →
  `isRejoinable()` (`:788`), which for a non-TLS-1.3 session is literally
  `sessionId != null && sessionId.length() != 0 && !invalidated &&
  isLocalAuthenticationValid()`. So the design decision in §3 — one function
  behind both accessors — mirrors HotSpot instead of approximating it. It also
  confirms arm E from the other side: `invalidated` is a plain field that only
  `invalidate()` writes, which is *why* invalidation changes exactly one
  answer.
  *Nuance worth recording:* under TLS 1.3 `isRejoinable` drops the id clause
  entirely (`useTLS13PlusSpec()` branch), so on HotSpot a valid TLS 1.3 session
  may have an empty id. CratonVM's negotiated sessions carry both, so the two
  do not diverge here — but a future "make `getId` real" change must not
  re-derive validity from it.
* **`getApplicationBufferSize()` is not a constant, and 16384 is not the JDK's
  answer.** `SSLSessionImpl.java:1297` computes
  `cipherSuite.calculateFragSize(maximumPacketSize, …)` when a packet size is
  set, and otherwise returns `SSLRecord.maxRecordSize - SSLRecord.headerSize`.
  With `headerSize = 5` (`SSLRecord.java:35`) and the measured
  `getPacketBufferSize() = 16709`, that is **16704** — E12-1's measured
  null-session value, reproduced from source. The deleted comment's claim
  ("the real JDK returns 16384, `SSLRecord.maxDataSize`") named a constant that
  this method only reaches **for DTLS**. The value is left at 16384
  deliberately; the assertion is what was wrong. (NOMINATION D for the twin.)

## 5. THE UNMASKING CHECK — every list-returning accessor in this file

E12-1 recorded that fixing the null-session return alone made
`getEnabledProtocols()` report `["NONE"]`, which is worse than the bug. That is
a *sentinel leaking into a configuration-shaped accessor*, so before landing I
checked every list-returning accessor in `t27_tls.rs` for the same exposure —
the question being "does it read the SESSION, or engine/socket configuration?"

| accessor (in `t27_tls.rs`) | reads | exposed? |
|---|---|---|
| `SSLEngineImpl.getEnabledProtocols` | `EngineState.enabled_protocols` (default `["TLSv1.3","TLSv1.2"]`) | **no** |
| `SSLEngineImpl.getEnabledCipherSuites` | `EngineState.enabled_ciphers`, else a 3-suite literal | **no** |
| `SSLEngineImpl.getSupportedProtocols` | hardcoded three | **no** |
| `SSLEngineImpl.getSupportedCipherSuites` | `SUPPORTED_CIPHER_SUITE_NAMES` | **no** (and a unit test now pins the sentinel out of that list) |
| `SSLEngineImpl.getSSLParameters` | `EngineState.enabled_*` | **no** |
| `SSLServerSocket.getEnabledProtocols` / `getSupportedProtocols` | side table, default `["TLSv1.3","TLSv1.2"]` | **no** |
| `SSLSocket.getApplicationProtocol` | negotiated ALPN | **no** |
| `SSLSession.getValueNames` | the attribute map slot | **no** (returns `String[0]`) |

**No list-shaped accessor in this file reads a session's protocol or cipher
slot**, so this change cannot repeat E12-1's site-8 unmasking. The reason is
structural and worth stating: in `t27_tls.rs` configuration lives in
`EngineState`/side tables and negotiation lives in the session object, and the
two are never crossed. `ssl_security.rs`'s `getEnabledProtocols` *did* cross
them, which is why the unmasking happened there and not here.

## 6. The fixture — which checks flip, and the one that still aborts

`regression-suite/src/RSslNullSession.java`, 47 checks, PASS on HotSpot. Not
edited (not this lane's file). PREDICTED CratonVM:

**Today (post-E12, pre-E22), DOOR 1 dies on check 2.** Line 146 is
`ck("socket.getHandshakeSession", String.valueOf(s.getHandshakeSession()), "null")`.
The dump has **no** registration for `javax/net/ssl/SSLSocket.getHandshakeSession`,
so the real JDK body runs and it is `throw new UnsupportedOperationException()`
(`jdk25src`, `SSLSocket.java:474`). Uncaught, `main` aborts, and the 45 checks
after it never execute. E12-1 §6 predicted "DOOR 1 passes" after its edits;
that is wrong, and it is wrong for a reason neither lane's edits touch —
**NOMINATION A** is the unblocker.

With NOMINATION A landed, the 9 checks **this change** flips:

| check | before | after (PRED) |
|---|---|---|
| `socket.getId.length` | `32` | **`0`** |
| `socket.isValid` | `true` | **`false`** |
| `socketClosed.getId.length` | `32` | **`0`** |
| `socketClosed.isValid` | `true` | **`false`** |
| `engine.getHandshakeSession` | non-null | **`null`** |
| `engine.getCipherSuite` | `TLS_AES_256_GCM_SHA384` | **`SSL_NULL_WITH_NULL_NULL`** |
| `engine.getProtocol` | `TLSv1.3` | **`NONE`** |
| `engine.getId.length` | `32` | **`0`** |
| `engine.isValid` | `true` | **`false`** |

Checks this change does **not** touch and which are predicted already green:
`socket.getCipherSuite`/`getProtocol` (E12-1's producer edits, live),
`socket.session = non-null` (edit 1, live), both `getPeerCertificates`
(t27's, empty chain → `SSLPeerUnverifiedException`), both `getPeerPrincipal`
(`ssl_security`'s, same), `getLocalCertificates`/`getLocalPrincipal` (`null`),
`getPacketBufferSize` (`16709`), `getValueNames` (`array[0]` — the 3-field
shape's last slot holds an `Int`, so the map lookup misses and returns empty),
`getId.stable` (`Arrays.equals(byte[0], byte[0])`), both
`getEnabledProtocols` checks (E12-1 site 8), and the three `unofferable`
checks (`SUPPORTED_CIPHER_SUITE_NAMES` contains both AES literals and not the
sentinel — now also a unit test in this file).

**Denominator note, stated honestly.** The brief asked me to verify Tomcat's
`JSSESupport.java:161`/`:171` myself. **Tomcat is not checked out on this
host** (`grep -rn JSSESupport` over the tree hits only E12-1's own record; no
`corpora` manifest and no Tomcat directory under `C:\craton`). So I did not
re-read those two lines, and this record does not claim to have. What I can
state on my own evidence is the CratonVM half, which is what the harm depends
on: after this change an unhandshaken session answers `getId().length == 0`
(so a `length == 0` test succeeds in suppressing tracking) and
`getCipherSuite() == "SSL_NULL_WITH_NULL_NULL"` (which, being absent from
`SUPPORTED_CIPHER_SUITE_NAMES` and from JSSE's `Cipher.values()` names, misses
any key-size map built from them, so no `SSL_CIPHER_USEKEYSIZE` can be
published for a session that negotiated nothing). Re-reading Tomcat is
**NOMINATION F**.

## 7. Where the family fix was DELIBERATELY not applied

NOMINATION 7 asked for six producer sites to become the two constants. Two of
them must not, and the reasoning is E12-1 §0's own ("any fix that treats them
as one family is wrong"), applied to a row it did not separate:

`tls_accept_server`'s `TlsServerConfig::Native` and `::LegacyDsa` arms
(`t27_tls.rs`, the two `"UNKNOWN".to_string()` tuples) are reached **by
succeeding**. `acceptor.accept(tcp)` returned; the stream is encrypted; a suite
genuinely was negotiated — native-tls 0.2 simply exposes no accessor to name
it. The four rustls arms are the opposite: they are reached when the handshake
succeeded *and rustls reports no suite*, an internal inconsistency for which
"nothing was negotiated" is honest.

Writing `SSL_NULL_WITH_NULL_NULL` on the native-tls arms would assert "no
cipher" about a live encrypted connection — a false statement in the
**dangerous** direction, the same direction as the fabrication this lane
removed, just inverted. `"UNKNOWN"` is not JSSE vocabulary either, but it is
*loud* rather than *plausible*, which is the correct trade when the truth is
unavailable. Both sites now carry that reasoning inline so the next reader does
not "complete the family". NOMINATION E is the accessor that would make it
knowable.

## 8. Residuals

1. **Nothing here was built or run.** No `cargo`, no VM, no fixture. The Rust
   was reviewed by hand against the trait signatures in
   `native-api/src/registry.rs` (`get_field`/`object_num_fields` are `&self`,
   `new_array`/`create_string`/`alloc_object` are `&mut self`) and against the
   identical shapes already compiling in `ssl_security.rs`.
2. **`getCreationTime` is now real but not stable.** HotSpot captures it in the
   constructor (§4), so two reads of one session agree; the narrow shapes here
   have no slot to hold it, so they answer `now` on every call. A drifting real
   timestamp beats a stable impossible one — every consumer computes
   `now - creationTime`, which goes from "56 years" to "~0 ms" — but it is a
   divergence, and NOMINATION C is the fix.
3. **`getId()`'s 32 bytes are still a CratonVM stand-in** for sessions that
   *did* negotiate. This lane removed the fabrication for sessions that did
   not; it did not make the positive case real. The VM does not surface the
   negotiated session id, so this is a capability gap, not a fallback bug.
4. **`getHandshakeSession` is now gated on `is_handshaking()` and this was not
   run.** E12-1 flagged it as "needs a run, not just an edit" because of
   Jetty's `SslConnection.getBufferSize()`. Jetty is not checked out here
   either, so I argued it from the oracle instead, and I think the argument is
   stronger than reading Jetty would have been: **HotSpot returns `null` here
   for every brand-new connection, which is exactly when Jetty calls it**
   (`SSLEngineImpl.getHandshakeSession()` is null outside the handshake window;
   the base `javax.net.ssl.SSLEngine` body throws outright, `SSLEngine.java:1085`).
   Any caller that works on HotSpot already tolerates null at that call site.
   The CratonVM NPE this registration exists for was a different one — real
   `SSLEngineImpl` dereferencing a `conContext` this VM never populates — and
   it is fixed by the registration *existing*, not by what it returns.
5. **The GC pin in `build_synthetic_ssl_session` is a separate fix in the same
   hunk.** `ses` was unpinned across three `create_string` calls, each a GC
   point; a moving young collection there sends every `set_field` through a
   stale reference. `ssl_security::new13_alloc_null_ssl_session` pins for
   exactly this reason and its comment calls the hazard "latent while this ran
   only at socket-construction time" — this constructor runs on *every*
   `getSession()`/`getHandshakeSession()` call, so it is the live version of
   that latent bug. Flagged separately so it can be dropped independently if a
   reviewer wants this commit to be one thing.
6. **`ssl_security::getPeerPrincipal` reads slot 2 as a stream id on any shape
   wider than 2 fields**, which on the 8-field engine shape is the `isValid`
   flag. Before this change that flag was always `1`, so it always queried
   stream 1; now it queries stream 1 or stream 0. Neither is right and the bug
   is pre-existing, but my change moves which wrong stream it asks about, so it
   is disclosed here rather than left for someone to bisect onto this commit.
   NOMINATION B.
7. **`putValue` on a 3-field session overwrites the stream id.** The attribute
   map goes in slot `num_fields - 1`, which on the NEW-13 shape *is*
   `NEW13_SESS_TLSID`. `session_has_negotiated` takes the defensive branch
   (non-`Int` at slot 2 → keep the pre-E12 answer) so this change cannot turn
   the corruption into a *new* wrong answer, but the corruption is real.
   NOMINATION G.

## How to verify

Cheapest first. Every CratonVM row is PREDICTED.

1. **`cargo test -p cratonvm-native-builtins t27_tls`** — four new unit tests,
   no VM, no network. `the_null_socket_session_has_no_id_and_is_not_valid` is
   the one that would have caught NOMINATION 3's gap; its mutation check
   `a_session_that_negotiated_keeps_its_id_and_its_validity` is the one that
   stops both accessors becoming unconditional. **Run the mutation check the
   mutation way**: make `session_has_negotiated` `return false` and the second
   test must fail while the first still passes.
2. **`bash regression-suite/run.sh` with `ONLY="RSslNullSession"`** — only
   after NOMINATION A, or DOOR 1 aborts on check 2 and reports nothing about
   this change. §6 lists the 9 checks that must flip.
3. **`--dump-native-registry`** (flags BEFORE `-cp`) — re-confirm §1's
   `owns_slot` column. If `ssl_security.rs` ever wins those slots back, this
   file's fixes become the inert ones and E12-1's become live; both are now
   correct, so that would be safe, but the *tests* would then be measuring dead
   code in whichever file lost.
4. **Any embedded-HTTPS fixture** — after a real handshake nothing here should
   change at all. Every edit is on a path that only runs when nothing was
   negotiated. A behaviour change on a successful connection means one of these
   fallbacks was being reached in the success path, which is a finding in its
   own right.

---

## NOMINATION A — `native-builtins/src/phases_late/ssl_security.rs`: `SSLSocket.getHandshakeSession` is unregistered and the JDK body throws

**This is the blocker for `RSslNullSession`, and neither E12-1 nor E3-1 saw
it.** The dump has no `javax/net/ssl/SSLSocket.getHandshakeSession` entry, so
the real JDK body runs:

```java
// jdk25src/java.base/javax/net/ssl/SSLSocket.java:474
public SSLSession getHandshakeSession() {
    throw new UnsupportedOperationException();
}
```

HotSpot never reaches that body because `SSLSocketImpl` overrides it and
returns `null` outside a handshake (measured, E12-1 §1; the fixture asserts
`"null"`). CratonVM's socket is a synthetic `javax/net/ssl/SSLSocket`, so it
*does* reach it.

ADD, next to the existing `getSession` registration (`ssl_security.rs`, in the
same registrar that registers `SSLSocket.getSession`):

```rust
    // E22: `javax.net.ssl.SSLSocket.getHandshakeSession()` has a CONCRETE body
    // in the JDK and it is `throw new UnsupportedOperationException()`
    // (jdk25src java.base/javax/net/ssl/SSLSocket.java:474). HotSpot never
    // reaches it because `SSLSocketImpl` overrides it; CratonVM's socket IS a
    // `javax/net/ssl/SSLSocket`, so an un-intercepted call throws where the
    // oracle answers. Measured (E12-1 §1): the answer is `null` outside a
    // handshake, which is the only state this VM's socket door can be in when
    // asked — `startHandshake()` here runs the whole handshake synchronously,
    // so there is no window in which a caller can observe a mid-handshake
    // session through this accessor.
    r.register(
        ssl_socket,
        "getHandshakeSession",
        "()Ljavax/net/ssl/SSLSession;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
```

(Substitute this registrar's local name for the `javax/net/ssl/SSLSocket`
class-name binding.) Without it, `RSslNullSession` reports nothing about
either lane's work.

## NOMINATION B — `ssl_security.rs`: `getPeerPrincipal` reads the 8-field shape's `isValid` flag as a stream id

`getPeerPrincipal` (and the now-dead `isValid`/`getId` beside it) do:

```rust
        let tls_id = if ctx.object_num_fields(this) > NEW13_SESS_TLSID {
            ctx.get_field(this, NEW13_SESS_TLSID).as_int().unwrap_or(-1)
        } else {
            -1
        };
```

`NEW13_SESS_TLSID` is `2`, so this fires on **any** shape with 3+ fields — but
slot 2 is the stream id only on the 3- and 4-field shapes. On the 8-field
engine shape it is the `isValid` flag, so an engine session queries
`s2_tls_peer_cert_chain_der(0)` or `(1)`: real ids in a registry that belongs
to a different shape's sockets. Cross-talk, not merely a miss.

REPLACE (in each of the three sites carrying this block):

```rust
        let tls_id = if ctx.object_num_fields(this) > NEW13_SESS_TLSID {
```

WITH:

```rust
        // E22: slot 2 is `NEW13_SESS_TLSID` only on the NEW-13 shape. On the
        // 8-field `t27_tls::build_synthetic_ssl_session` shape it is the
        // `isValid` flag, so a `> NEW13_SESS_TLSID` test reads a boolean as a
        // stream id and looks up whichever socket happens to own s2 stream
        // 0 or 1. Match the exact width instead — the same field-count
        // discrimination `t27_tls::session_has_negotiated` documents.
        let tls_id = if ctx.object_num_fields(this) == NEW13_SSL_SESS_FIELDS {
```

## NOMINATION C — `ssl_security.rs`: widen `NEW13_SSL_SESS_FIELDS` to carry a creation time

`getCreationTime`/`getLastAccessedTime` are now a real epoch for the narrow
shapes but not stable across reads (§8 residual 2), because the 3-field shape
has nowhere to stash one. HotSpot captures it in the constructor
(`SSLSessionImpl.java:167`).

REPLACE:

```rust
pub(crate) const NEW13_SSL_SESS_FIELDS: usize = 3;
```

WITH:

```rust
/// E22: was 3. Slot 3 holds the creation time in epoch millis, stamped at
/// construction so two reads of one session agree — which is what HotSpot
/// does (`sun/security/ssl/SSLSessionImpl.java:167`,
/// `creationTime = System.currentTimeMillis()` in the null-session ctor) and
/// what `t27_tls`'s accessors cannot do for a shape with no slot for it.
pub(crate) const NEW13_SSL_SESS_FIELDS: usize = 4;

/// Epoch millis, stamped once at construction. See `NEW13_SSL_SESS_FIELDS`.
pub(crate) const NEW13_SESS_CREATED: usize = 3;
```

and write `Value::Long(crate::epoch_millis_now())` into `NEW13_SESS_CREATED`
in **both** `new13_alloc_null_ssl_session` and `new13_alloc_ssl_session` (and
in the 3-field allocation at the third site, which must widen with them).

**Ordering caution:** this widens the shape from 3 to 4, and 4 is already the
`SSLServerSocket.accept` width. `t27_tls::session_has_negotiated`'s `3 =>` arm
tests `slot2 >= 0` and its `_ =>` arm (which 4 falls into) answers `true`
unconditionally, so **widening to 4 without also updating that function turns
the null session valid again** — the exact defect this lane just fixed. Land
them together, or widen to 5.

## NOMINATION D — `native-builtins/src/tls.rs`: the same wrong buffer-size comment

`tls.rs` (~`:1108`-`:1136`) carries the twin of the "KEEP (correct
constants…) the real JDK returns 16384" header that this lane corrected in
`t27_tls.rs`. Apply the same correction — the values stay, the claim goes. The
derivation is now sourced, not just measured: `SSLSessionImpl.java:1297`
returns `SSLRecord.maxRecordSize - SSLRecord.headerSize` = `16709 - 5` =
**16704** for a session with no `maximumPacketSize` and no negotiated max
fragment length, and `cipherSuite.calculateFragSize(...)` = **16676** after a
TLS 1.3 handshake. `SSLRecord.maxDataSize` — the constant the old comment
named — is only reached on the DTLS branch.

## NOMINATION E — `t27_tls.rs` (this lane's file, deliberately deferred): name the native-tls acceptor's suite

§7's two arms answer `"UNKNOWN"` because native-tls 0.2 has no cipher
accessor. Two ways out, neither cheap enough for this commit: (a) drop the
legacy acceptors and route everything through rustls, which is a behaviour
change for the DSA/legacy fixtures those arms exist for; (b) read the suite
out of the underlying `SslStream` via the backend-specific escape hatch
(`native_tls::TlsStream::raw`-equivalent per platform), which is three
platform implementations. Recorded so the `"UNKNOWN"` is a known gap rather
than an oversight.

## NOMINATION F — re-verify the Tomcat denominator when Tomcat is checked out

E12-1 §4 names `JSSESupport.java:161` and `:171` as the two consumers this
change decides. **Neither this lane nor a reader on this host can check them:
Tomcat is not present** (no checkout under `C:\craton`, no manifest, and the
only in-tree hit for `JSSESupport` is E12-1 itself). The claim is plausible
and load-bearing, so it should be confirmed once rather than cited twice.
Specifically: confirm `:171` is a `length == 0` test and not a `length < N`
test, and confirm `:161`'s `keySizeCache` is keyed on JSSE suite names (if it
is keyed on anything normalised, the "sentinel misses the map" conclusion
needs re-deriving).

## NOMINATION G — `t27_tls.rs` (this lane's file, deliberately deferred): the attribute slot collides with the stream id

`putValue`/`getValue`/`getValueNames`/`sslsess_attrs_map` use slot
`num_fields - 1` as the attribute map. That is a dedicated slot on the 4-field
and 8-field shapes (both documented as carrying one) and **is not** on the
others: on the 3-field NEW-13 shape it is `NEW13_SESS_TLSID`, and on `tls.rs`'s
6-field shape it is the creation time. A `putValue` on either destroys that
field.

Not fixed here because the fix is a behaviour change on a path unrelated to the
null session (the honest options are "no-op the attribute API for shapes with
no attrs slot" or "widen those shapes"), and because this lane already carries
one adjacent fix (§8 residual 5). `session_has_negotiated` takes a defensive
branch so this change cannot make the corruption worse. The fix:

```rust
/// The dedicated attribute-map slot, or `None` for a shape that has none.
/// `num_fields - 1` is only the attrs slot on the two shapes documented as
/// carrying one; on the 3-field NEW-13 shape it is `NEW13_SESS_TLSID` and on
/// `tls.rs`'s 6-field shape it is the creation time, and `putValue` silently
/// destroys either.
fn sslsess_attrs_slot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    match ctx.object_num_fields(this) {
        4 => Some(3),
        n if n >= 7 => Some(n - 1),
        _ => None,
    }
}
```

with `getValue`/`getValueNames` returning null/`String[0]` and `putValue`/
`removeValue` no-opping when it is `None`.
