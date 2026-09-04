# G7-1 — the SSLSession surface measured end to end, and the three questions the merge left open

> **RECONCILED 2026-08-17 (lane G40) — the provenance premise "no JDK source was
> read" was avoidable.** `C:\craton\jdk25src` is indeed absent, and this record
> is right about that. But the JDK's sources ship with the oracle itself, at
> `$JAVA_HOME/lib/src.zip` (52,462,198 bytes) — the sources of the exact
> HotSpot 25.0.3+9-LTS build used here, `javax.net.ssl` included. Nothing
> measured in this record is invalidated. `RSslLiveSession` was still red at
> `9964ca733`; see `G16-1` and `BASELINE-20260817.md`. See `INDEX.md` §B.3.

**Status:** FIXED-UNVERIFIED (`native-builtins/src/t27_tls.rs`,
`native-builtins/src/http_url_connection.rs`,
`native-builtins/src/phases_late/ssl_security.rs` — this lane's three files).
NOMINATED (everything else).
**Prov:** HotSpot column MEAS (this host). CratonVM column **NOT MEASURED AT
ALL**.
**2026-08-16, lane G7.** Branch `claude/jdk-only-mode-completion-1351c0`, on top
of the `dev` merge `d87dff06a`.

> **NOTHING IN THIS RECORD WAS MEASURED ON A CRATONVM BINARY.** No binary
> carrying these changes exists; the orchestrator was building while this lane
> ran, and this lane was forbidden to build. Every CratonVM "after" below is a
> claim about source. The HotSpot column is real: it was produced by running
> `scratchpad/g7/TlsProbe.java` on the oracle, and it is transcribed, not
> derived.

Oracle: HotSpot 25.0.3+9-LTS, Temurin, `$JAVA_HOME`.
Probe: `scratchpad/g7/TlsProbe.java`, single-file source mode, driving a real
loopback `SSLServerSocket` handshake, a paired in-memory `SSLEngine`, and a real
`HttpsURLConnection` against the same server. Labels are ASCII throughout
(HANDOFF-20260814 §7). `C:\craton\jdk25src` is **absent** on this host, so every
JDK-source claim in this record comes from `javap` on the oracle instead, and is
labelled SOURCE-VERIFIED where it does.

---

> **VERIFIED AGAINST A BINARY 2026-09-04.** The provenance line read *"CratonVM
> column **NOT MEASURED AT ALL**"*. §5's two shadowing findings are ownership
> claims, and a `--dump-native-registry` run on a binary built from this tree
> settles both **exactly as written**.
>
> **§5.2 — "Seven of `ssl_security.rs`'s ten `SSLSession` doors are dead."**
> Ten registrations, three own their slot, seven do not:
>
> ```text
> live (owns_slot=true)   getLocalCertificates  getLocalPrincipal  getPeerPrincipal
> dead (owns_slot=false)  getCipherSuite  getCreationTime  getId  getLastAccessedTime
>                         getPeerCertificates  getProtocol  isValid
> ```
>
> Ten, three, seven. The in-tree unit test
> `ssl_security::new13_tests::seven_of_this_files_ssl_session_doors_are_dead_and_three_are_live`
> also passes, so the claim now holds from both directions — a source witness
> and a live registry.
>
> **§5.1 — "`http_url_connection.rs` overwrites `net_phase_e.rs` for five of six
> HTTPS accessors."** `net_phase_e.rs` registers exactly six on
> `HttpsURLConnection`. Five lose the slot; the sixth keeps it:
>
> ```text
> getCipherSuite         net_phase_e.rs:9656  owns=false   -> http_url_connection.rs:665
> getServerCertificates  net_phase_e.rs:9669  owns=false   -> http_url_connection.rs:625
> getLocalCertificates   net_phase_e.rs:9692  owns=false   -> http_url_connection.rs:653
> getPeerPrincipal       net_phase_e.rs:9710  owns=false   -> http_url_connection.rs:695
> getLocalPrincipal      net_phase_e.rs:9728  owns=false   -> http_url_connection.rs:724
> getSSLSession          net_phase_e.rs:9746  owns=TRUE    -- the sixth, not overwritten
> ```
>
> Five of six, and the note now names which one survives:
> `getSSLSession`. `HttpsURLConnectionImpl` shows the same five, the same way.
>
> **What this does NOT verify — and it is most of this record.** §§1-1e are a
> MEASURED HotSpot surface: every accessor in every session state, `SSLEngine`
> paired in memory, `HttpsURLConnection` end to end. That is the oracle, and
> **none of it was re-run against CratonVM here.** A registry dump says which
> function answers a call; it says nothing about what that function returns, so
> the three MERGE QUESTIONS in §§2-4 — `getSessionContext()`,
> `getHandshakeSession()` pre-handshake, `getId()` mid-handshake — remain
> unmeasured on this VM. `RSslLiveSession` and `RSslNullSession` both pass in
> Compatible and `--jdk-only`, which is consistent with those answers being
> right and is not evidence that they are.

## 0. The headline

| merge question | the merge's position | the oracle | outcome |
|---|---|---|---|
| 1. `getSessionContext()` returns `null` unconditionally | deliberate under-report; "this VM has no SSLSessionContext" | non-null after a completed handshake, `null` in three other states | **the premise was false.** Implemented, gated, test rewritten |
| 2. `getHandshakeSession()` returns `null` pre-handshake | correct; dev's Jetty NPE attribution rebutted | `null` on every pre-handshake state, socket and engine alike | **the merge is right.** No code change |
| 3. `getId()` answers `byte[0]` mid-handshake | two stacked gates | `byte[32]`, and byte-identical to the completed session's id | **the second gate was wrong here.** Dropped, and moved to question 1's door |

The three answers turn out to be one answer. `session_has_negotiated` ("a suite
has been agreed") and `session_is_negotiated` ("this session object belongs to a
COMPLETED handshake epoch") are different predicates, and the merge had them on
the wrong doors: `getId` does not care about the difference and
`getSessionContext` is defined by it.

---

## 1. The measured surface — every accessor, every state (MEASURED)

Transcribed from `scratchpad/g7/TlsProbe.java`. Client side unless stated.
`(no ctx)` = `getSessionContext()` answered `null`.

| accessor | never-connected | mid-handshake (inside `checkServerTrusted(..,Socket)`) | completed | invalidated | resumed |
|---|---|---|---|---|---|
| `<class>` | `SSLSessionImpl` | `SSLSessionImpl` | `SSLSessionImpl` | `SSLSessionImpl` | `SSLSessionImpl` |
| `getId` | `byte[0]` | **`byte[32]`** `2310720247a7e833…` | `byte[32]` **same 32 bytes** | `byte[32]` unchanged | `byte[32]` **different** |
| `getCipherSuite` | `SSL_NULL_WITH_NULL_NULL` | `TLS_AES_256_GCM_SHA384` | `TLS_AES_256_GCM_SHA384` | survives | `TLS_AES_256_GCM_SHA384` |
| `getProtocol` | `NONE` | `TLSv1.3` | `TLSv1.3` | survives | `TLSv1.3` |
| `getPeerHost` | `null` | `127.0.0.1` | `127.0.0.1` | survives | `127.0.0.1` |
| `getPeerPort` | `-1` | real port | real port | survives | real port |
| `getCreationTime` | real epoch (>0) | >0 | >0 | >0 | >0 |
| `getLastAccessedTime` | real epoch (>0) | >0 | >0 | >0 | >0 |
| `isValid` | `false` | **`true`** | `true` | **`false`** | `true` |
| `invalidate()` | returns, changes nothing | — | moves `isValid` and `getSessionContext`, nothing else | idempotent | — |
| `getSessionContext` | `null` | **`null`** | `SSLSessionContextImpl` | **`null`** | `SSLSessionContextImpl` |
| `getPeerCertificates` | THROWS `SSLPeerUnverifiedException: peer not authenticated` | **THROWS**, same | 1 cert | 1 cert (survives) | 1 cert |
| `getLocalCertificates` | `null` | `null` | `null` | `null` | `null` |
| `getPeerPrincipal` | THROWS `SSLPeerUnverifiedException: peer not authenticated` | **THROWS**, same | `X500Principal[CN=localhost,OU=craton,O=craton,C=US]` | survives | same |
| `getLocalPrincipal` | `null` | `null` | `null` | `null` | `null` |
| `getValueNames` (fresh) | `String[0]` | `String[0]` | `String[0]` | `String[0]` | `String[0]` |
| `putValue`/`getValue` round-trip | works | works | works | works | works |
| `putValue(null,v)` / `(k,null)` | THROWS `IllegalArgumentException: arguments can not be null` | same | same | same | same |
| `getValue(null)` / `removeValue(null)` | THROWS `IllegalArgumentException: argument can not be null` | same | same | same | same |
| `removeValue(absent)` | returns normally | same | same | same | same |
| `getPacketBufferSize` | `16709` | `16709` | `16709` | `16709` | `16709` |
| `getApplicationBufferSize` | **`16704`** | **`16676`** | `16676` | `16676` | `16676` |

Three rows deserve stating on their own, because they are the ones a reading of
the code would not have produced:

- **Mid-handshake is not "half a null session".** It is a fully populated
  session — id, suite, protocol, peer host/port, `isValid() == true` — with
  exactly two things missing: the peer's certificate (we are *inside* the code
  that decides whether to accept it) and the session context. The plural/
  singular split in the attribute messages holds there too.
- **`getApplicationBufferSize` moves and `getPacketBufferSize` does not.**
  16704 before a suite is negotiated, 16676 after; the packet size is 16709 in
  every state. This VM answers a flat `16384` for both, deliberately (see
  `t27_tls`'s comment). Confirms E12-1 §1 and E22-1 §4 on a second host.
- **`invalidate()` moves exactly two things.** `isValid()` and
  `getSessionContext()`. The id keeps its 32 bytes and its exact contents, the
  suite, protocol, peer principal and peer chain all survive, and
  `context.getSession(id)` goes from SAME-OBJECT to `null`.

### 1b. The `SSLSessionContext` itself (MEASURED)

| | before any handshake | after one completed handshake |
|---|---|---|
| class | `sun.security.ssl.SSLSessionContextImpl` | same |
| `getSessionCacheSize` | `20480` | `20480` |
| `getSessionTimeout` | `86400` | `86400` |
| `getIds()` count | `0` | `1` |
| `getSession(id)` | — | **SAME OBJECT** as `socket.getSession()` |
| `getSession(new byte[0])` | `null` | `null` |
| `getSession(null)` | THROWS `NullPointerException: session id cannot be null` | same |
| after `invalidate()` | — | `getSession(id)` -> `null` |

### 1c. `SSLEngine`, paired in memory (MEASURED)

```text
  step  0 cstat=NEED_WRAP       client.getHandshakeSession=null
  step  1 cstat=NEED_UNWRAP     client.getHandshakeSession=null
  step  2 cstat=NEED_TASK       client.getHandshakeSession=null
  step  3 cstat=NEED_UNWRAP     client.getHandshakeSession=populated id=32B cs=TLS_AES_256_GCM_SHA384
  step  4 cstat=NEED_TASK       client.getHandshakeSession=populated id=32B cs=TLS_AES_256_GCM_SHA384
  step  5 cstat=NOT_HANDSHAKING client.getHandshakeSession=null
```

A never-begun engine: `getHandshakeStatus() = NOT_HANDSHAKING`,
`getHandshakeSession() = null`, `getSession()` = the null session (`byte[0]`,
`SSL_NULL_WITH_NULL_NULL`, `NONE`, `isValid=false`, appBuf **16704**). After the
handshake the engine's `getSession()` is a **different object** from the one it
answered before it — the same non-identity this VM models with
`engine_session_for`'s `(engine, handshaked)` cache key.

### 1d. `HttpsURLConnection` (MEASURED)

| accessor | before `connect()` | after `connect()` | after `disconnect()` |
|---|---|---|---|
| `getCipherSuite` | `IllegalStateException: connection not yet open` | `TLS_AES_256_GCM_SHA384` | `IllegalStateException: connection not yet open` |
| `getServerCertificates` | same ISE | 1 cert | same ISE |
| `getLocalCertificates` | same ISE | `null` | same ISE |
| `getPeerPrincipal` | same ISE | `X500Principal[CN=localhost,…]` | same ISE |
| `getLocalPrincipal` | same ISE | `null` | same ISE |
| `getSSLSession` | same ISE | `Optional[SSLSessionImpl]` | same ISE |

Connection class is `sun.net.www.protocol.https.HttpsURLConnectionImpl`. The
accessors **do not connect on demand**: the trust-manager callback fired at
`connect()`, not while the pre-connect accessors were being called. The
post-`disconnect()` row is the same message as the pre-connect one, so the
message describes the state and not the call order.

SOURCE-VERIFIED, `javap -p javax.net.ssl.HttpsURLConnection` on the oracle:

```text
  public abstract java.lang.String getCipherSuite();
  public abstract java.security.cert.Certificate[] getLocalCertificates();
  public abstract java.security.cert.Certificate[] getServerCertificates()
          throws javax.net.ssl.SSLPeerUnverifiedException;
  public java.security.Principal getPeerPrincipal()
          throws javax.net.ssl.SSLPeerUnverifiedException;
  public java.security.Principal getLocalPrincipal();
  public java.util.Optional<javax.net.ssl.SSLSession> getSSLSession();
```

Two things follow. `getCipherSuite`, `getLocalCertificates` and
`getLocalPrincipal` declare **no checked exception at all**. And
`getSSLSession()` is **not abstract** — `javap -c` shows its body is
`Optional.empty(); areturn` — so an un-intercepted call there returns an empty
Optional rather than throwing `AbstractMethodError`, unlike its five siblings.

### 1e. One more, for the record

`new SSLSocket(){}.getHandshakeSession()` on the abstract base throws
`java.lang.UnsupportedOperationException` with a **null** message. Confirms
E31-1 §1's ARM0 on a second host.

---

## 2. MERGE QUESTION 1 — `getSessionContext()`. The premise was false; implemented

**The merge's comment said:** *"This VM has no `SSLSessionContext` — no session
cache, no id-keyed lookup, no timeout — so 'unavailable' is a true statement
about it."*

**SOURCE-VERIFIED, and it is not.** `net_phase_e::register_phase_e_networking`
allocates a zero-field `javax/net/ssl/SSLSessionContext` carrier for
`SSLContext.getClientSessionContext()` and `getServerSessionContext()`
(`net_phase_e.rs:13426`, `:13443`), and registers the **whole interface** on it
(`net_phase_e.rs:13466`-`13525`): `setSessionCacheSize(I)V`,
`setSessionTimeout(I)V`, `getSessionCacheSize()I`, `getSessionTimeout()I`,
`getIds()Ljava/util/Enumeration;`,
`getSession([B)Ljavax/net/ssl/SSLSession;`. The object exists, it has a live
method surface, and applications already receive it through the other two doors.

So the `null` was not `--jdk-only` restraint about a capability the VM lacks. It
was one door disagreeing with its two siblings about whether the VM has a
context at all. The interface's escape hatch ("This context may be unavailable
in some environments, in which case this method returns null") is still the
right answer for the three states that measure `null`; it does not license
claiming unavailability for a completed handshake while
`SSLContext.getClientSessionContext()` hands the same application a context one
call away.

**Landed** (`t27_tls.rs`, `register_ssl_session_real`): the body mints the same
zero-field carrier when — and only when —

```rust
let valid = session_is_valid(ctx, this);                  // negotiated && !invalidated
let engine_shape = ctx.object_num_fields(this) >= 7;
if !valid || (engine_shape && !session_is_negotiated(ctx, this)) {
    return Ok(Some(Value::Object(None)));
}
```

Against the oracle's four states: never-negotiated `null` (gate 1),
mid-handshake `null` (gate 2), completed non-null, invalidated `null` (gate 1).
**Four of four.** The second gate applies only to the 7/8-field engine shape,
because that is the only shape whose session object can be handed to an
application while a handshake is running — the socket door
(`register_socket_handshake_session`) answers `null` for its entire handshake
window, so no width-4 session is observable mid-handshake.

Dev's dropped registration returned a zero-field carrier **unconditionally**.
That is right for netty's `SSLEngineTest.testSessionAfterHandshake0` (which
asserts non-null) and wrong for the three states that measure `null`, including
the null session `RSslNullSession` is built around. The gated form satisfies
both.

**Test rewritten in the same edit,** as required:
`get_session_context_answers_null_rather_than_fabricating_one` no longer asserts
`null` for both a negotiated and a never-negotiated shape. It now pins three
arms (never-negotiated `null`, negotiated non-null, invalidated `null`), and a
new sibling `a_handshake_still_in_flight_has_no_session_context_yet` pins the
engine shape's three states. The old doc comment's second sentence — the false
premise — is replaced by the correction rather than deleted.

**Two residuals, both net_phase_e's and both NOMINATED (N1, N2):** the carrier's
`getIds()` answers an empty enumeration and `getSession(id)` answers `null`
where HotSpot lists this session and returns the same object; and
`getSessionCacheSize()`/`getSessionTimeout()` answer net_phase_e's ORPHAN-key
default `0`/`0` where HotSpot measured `20480`/`86400`, because `ssc_bind` is
private to that file.

---

## 3. MERGE QUESTION 2 — `getHandshakeSession()` pre-handshake. The merge is right

The merged comment rebuts a dev comment that attributed a Jetty hang
(`SslConnection.getBufferSize()` NPE) to returning `null` there. **The rebuttal
is correct, and the oracle says so on every pre-handshake state there is:**

```text
  unconnected SSLSocket, getHandshakeSession()          -> null
  after getSession() on that socket                     -> null
  after close()                                         -> null
  never-begun SSLEngine (NOT_HANDSHAKING)               -> null
  engine steps 0,1,2 (before ServerHello)               -> null
  post-handshake socket AND engine                      -> null
  inside HandshakeCompletedListener                     -> null
  inside checkServerTrusted(chain, auth, Socket|SSLEngine) -> POPULATED
```

Jetty sizes buffers for a brand-new connection, which is the first and fourth
rows. HotSpot returns `null` there. Any caller that works on HotSpot therefore
already tolerates `null` at that call site, or it would NPE on real JSSE on
every connection it ever made. **No code change.** The original CratonVM defect
the registration exists for was a different NPE — real
`SSLEngineImpl.getHandshakeSession()` dereferencing a `conContext` this VM never
populates — and it is fixed by the registration EXISTING, not by what it
returns.

One thing the oracle adds that neither comment had: the non-null window closes
**before** `HandshakeCompletedListener` runs, not after. So the two callbacks an
application can install see opposite answers, and a fix that widened the window
to "until the handshake completes" would get the listener wrong.

---

## 4. MERGE QUESTION 3 — `getId()` mid-handshake. The second gate was on the wrong door

The merged `getId` stacked two gates:

```rust
if !negotiated || (engine_shape && !session_is_negotiated(ctx, this)) { byte[0] }
```

Enumerate what the second one adds over the first:

| state | slot 2 (`isValid` flag) | in `negotiated_session_keys` | gate 1 | gate 2 |
|---|---|---|---|---|
| fresh engine, no `conn` | `0` | no | **refuses** | (moot) |
| mid-handshake, past ServerHello | `1` | **no** | admits | **refuses** |
| completed | `1` | yes | admits | admits |

`build_synthetic_ssl_session` writes slot 2 from
`conn.negotiated_cipher_suite().is_some()`, and `engine_session_for` inserts
into `negotiated_session_keys` only when `handshaked`. So **the second gate
decided exactly one row, and on that row the oracle says 32 bytes.**

netty's `SSLEngineTest.testSSLSessionId` — the test the gate was added for —
asserts `assertEquals(0, engine.getSession().getId().length)` on a *freshly
created* engine, which is row 1, and gate 1 alone already answers it: no `conn`
means no negotiated suite means slot 2 is `0`. Dropping gate 2 cannot regress
it.

**Landed:** gate 2 removed from `getId`; `getId` is now
`session_has_negotiated` alone, which is what its own neighbouring comments
already said it was (`"getId` is deliberately gated on `session_has_negotiated`
ALONE"`, `session_is_valid`'s doc). **The gate was not deleted — it moved to
`getSessionContext`**, which is the door whose oracle answer actually changes
across that boundary. So neither `session_is_negotiated` nor
`negotiated_session_keys` goes dead: both keep exactly one reader, and the
reader is now the one that needs them. That answers the merge note's worry
directly.

**Residual, and it is real.** HotSpot's handshake session *is* the session: the
32 bytes read inside `checkServerTrusted` were byte-identical to the 32 bytes
the completed session then reported (`2310720247a7e833…` in both). Here
`engine_session_for` caches on `(engine, handshaked)`, so mid-handshake and
completed are **different objects**, and the pseudo-id — seeded from
`gc_stable_objref_key` — differs between them. The LENGTH is now right, which is
what Tomcat's `JSSESupport.getSessionId` tests (`length == 0` exactly); the
CONTINUITY is not. Merging the two cache entries is not free: the pre-handshake
entry has the "nothing negotiated" sentinels frozen into slots 0/1 at mint time,
so reusing that object after the handshake would make a completed session report
`SSL_NULL_WITH_NULL_NULL` — trading a length bug for a suite bug. Recorded here
rather than half-done.

---

## 5. The shadowing this lane found while checking its own registrations

Following HANDOFF-20260814 §5 ("a green build proves you broke nothing, not that
you did something") and C6-3's question, this lane audited which body wins for
every triple it touched. Two findings, one per file.

### 5.1 `http_url_connection.rs` overwrites `net_phase_e.rs` for five of six HTTPS accessors

There are **two functions named `register_https_session_accessors`**, one in
each file, registering the same names on the same two classes
(`sun/net/www/protocol/https/HttpsURLConnectionImpl` and
`javax/net/ssl/HttpsURLConnection`).

```text
  lib.rs:18688  net_phase_e::register_phase_e_networking
                  -> register_re4_url_http
                  -> register_https_session_accessors            (6 names, net_phase_e.rs:8303)
  lib.rs:18805  http_url_connection::register_http_url_connection_real
                  -> register_https_session_accessors(r, ...Impl)
                  -> register_https_session_accessors(r, ...HttpsURLConnection)
                                                                 (5 names, http_url_connection.rs:282)
```

Both calls are inside `register_essential_natives_with_shims`; 18805 runs after
18688; registration is last-write-wins. **So `http_url_connection.rs`'s bodies
own `getServerCertificates`, `getLocalCertificates`, `getCipherSuite`,
`getPeerPrincipal`, `getLocalPrincipal`, and net_phase_e's five copies are
dead.** `getSSLSession` is the one name `http_url_connection.rs` does not
register, so net_phase_e's survives for it alone.

net_phase_e's own doc comment reasons about this and rules it out:

> *"None of the six names below appear in `register_one`, so none of them can be
> overwritten by it. If a later change adds any of them there, THAT copy wins
> and this one goes silently dead — check the dump, not the source order."*

The premise is true of `register_one` and the conclusion is false, because the
names were added to a **second registrar in the same file** rather than to
`register_one`. The comment's own advice is the right advice and it was not
followed.

The consequence: **the six accessors answer from two different tables.** The
five live ones read `https_peer_info()` (this file, written by
`record_https_peer_info`); the surviving `getSSLSession` reads net_phase_e's
`https_carrier_session` (written by `record_https_carrier_session`, called from
`huc_verify_hostname` STEP 0 — C6-1's NOMINATION 1, which **did** land, contrary
to INDEX's "the populator is a NOMINATION, so every accessor answers 'not
open'"). Both populators run on the same successful exchange, so the split is
not observable today; it is one deleted call away from being so.

**Pinned:** new test
`this_files_registrar_owns_five_of_the_six_https_session_accessors` asserts the
five ARE registered by `register_http_url_connection_real` alone and that
`getSSLSession` is NOT — so adding `getSSLSession` here fails a build instead of
silently killing the only body that serves it.

### 5.2 Seven of `ssl_security.rs`'s ten `SSLSession` doors are dead

Mechanically derived by parsing both registrars:

```text
  DEAD (t27_tls::register_ssl_session_real runs later and wins)
      getProtocol  getCipherSuite  isValid  getId
      getPeerCertificates  getCreationTime  getLastAccessedTime
  LIVE (t27_tls deliberately does not re-register these)
      getPeerPrincipal  getLocalCertificates  getLocalPrincipal
```

Order: `phases_late.rs:6670` `register_p68_ssl` then `:6674`
`register_t27_natives`; `lib.rs:18665` then `:18726`. This confirms E22-1 §1's
`--dump-native-registry` census still holds after the `dev` merge, and it is
exactly the trap that made three of E12-1's eight edits inert.

**Pinned:** new test
`seven_of_this_files_ssl_session_doors_are_dead_and_three_are_live` compares
callback identity between a `register_p68_ssl`-only registry and one built in
the real order. A future lane that lands a session fix in one of the seven still
passes — the fix is simply inert, and that is unavoidable — but a lane that
MOVES one of the three now fails a build and is told which one.

### 5.3 The one registration this lane changed that cannot be shadowed

`javax/net/ssl/SSLSession.getSessionContext()Ljavax/net/ssl/SSLSessionContext;`
has **exactly one** production registration in the entire repository
(`t27_tls.rs`, `register_ssl_session_real`) — `grep -rn 'getSessionContext'`
over `native-builtins/src` and `vm/src` returns that site plus test-module
references and prose. It cannot be overwritten because there is nothing else to
overwrite it. That is how this lane established that the body it edited is the
one that runs.

---

## 6. The refusal type on the HTTPS accessors

MEASURED (§1d): all six throw `java.lang.IllegalStateException: connection not
yet open` before `connect()` and again after `disconnect()`.
`SSLPeerUnverifiedException` is the answer to a different question — the
connection IS open and the peer did not authenticate — and it is the exception
`getServerCertificates` and `getPeerPrincipal` declare.

The live bodies conflated the two: `https_peer_chain_or_throw` answered
`SSLPeerUnverifiedException: peer not authenticated` for *both* "no entry" and
"entry with an empty chain", and `getCipherSuite` did the same. For
`getCipherSuite`, `getLocalCertificates` and `getLocalPrincipal` that was an
**undeclared checked exception** through a `throws`-free method (SOURCE-VERIFIED
by `javap`, §1d) — an `IOException` subclass no `catch` written against this API
can name.

**Landed** in `http_url_connection.rs`:

- new `https_not_yet_open(ctx)` -> `IllegalStateException: connection not yet
  open`, matching the shape net_phase_e's identically-named helper already used;
- new `https_has_session(ctx, this)` — the discriminator: *no entry* means the
  exchange never completed, *an entry* means it did;
- `https_peer_chain_or_throw` now splits the two arms;
- `getCipherSuite`'s refusal becomes the ISE;
- `getLocalCertificates` and `getLocalPrincipal`, which answered a flat `null`
  in every state, now answer `null` **once the connection is open** (MEASURED:
  a client that sent nothing has neither) and the ISE before that. The flat
  `null` was the silent-lie shape: it answered "no local certificate was sent"
  for a connection that had not been opened.

**Behaviour change the orchestrator must watch:** `getLocalCertificates` and
`getLocalPrincipal` now route through `https_ensure_exchanged`, so they can
drive the same lazy exchange their four siblings already drove. Post-connect
answers are unchanged (`null`); only the pre-connect state moves, from a wrong
answer to the measured refusal.

**Residual, stated because it makes one arm reachable more often than it
should be:** `record_https_peer_info` early-returns when the chain is empty, so
an anonymous-suite handshake leaves no entry and lands on the ISE arm rather
than the `SSLPeerUnverifiedException` arm one line above. Recording the entry
unconditionally is that function's call contract and was left alone.

---

## 7. What the orchestrator must check at build time

1. `native-builtins` depends on `native-io`; if `native-io` fails, nothing here
   was checked at all (HANDOFF §5).
2. `t27_tls.rs`'s `getSessionContext` now calls
   `try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSessionContext", 0)`.
   The import already exists (`t27_tls.rs:70`). The 0-field request matches
   net_phase_e's two existing call sites exactly; `SSLSessionContext` is a real
   JDK interface declaring zero fields, and widening it would produce an
   undersized-layout object the GC bounds guard rejects. **Do not "fix" the 0.**
3. Three new tests allocate through `MockNativeContext`. Its
   `ensure_class_initialized` always succeeds and registers the name, so
   `try_alloc_concurrent_synthetic` resolves to a fresh ClassId with
   `class_num_total_fields == 0` and the caller's size stands. If that mock
   behaviour has changed, `get_session_context_answers_null_rather_than_fabricating_one`
   ARM 2 is where it will show.
4. `a_handshake_still_in_flight_has_no_session_context_yet` inserts into and
   then removes from the process-global `negotiated_session_keys`.
   `MockNativeContext` restarts its pointer sequence per instance, so two
   contexts can derive the same `gc_stable_objref_key`; the key is removed
   before the assertion for that reason. If this test flakes under
   `--test-threads`, that collision is why.
5. Run with `--dump-native-registry` once a binary exists and confirm:
   `javax/net/ssl/SSLSession.getSessionContext` `owns_slot=true` with
   `registered_by = t27_tls.rs`; the five HTTPS accessors `owns_slot=true` with
   `registered_by = http_url_connection.rs` and `overwrote` naming
   `net_phase_e`; `getSSLSession` `owns_slot=true` with `registered_by =
   net_phase_e.rs`. **§5.1 and §5.2 are SOURCE-VERIFIED from the call orders,
   not from a dump.**
6. `RSslNullSession` asserts `getSessionContext() = null` on the null session —
   still correct under this change (gate 1 refuses). `RSslLiveSession`'s
   `serverSide` family asserts a non-null context on a negotiated session —
   that row moves from red to green **if** it runs; it is not scheduled yet
   (F36-1 NOMINATION 1).

---

## 8. NOMINATIONS

Everything outside this lane's three files.

**N1 — `native-builtins/src/net_phase_e.rs`, `register_phase_e_networking`, the
`getIds`/`getSession` pair at `:13511` and `:13519`.** The
`SSLSessionContext` carrier answers an empty enumeration and a null lookup. Now
that `SSLSession.getSessionContext()` hands out that same carrier for a
negotiated session, "the context a session points at does not contain that
session" is reachable. **Change:** back both with a side table keyed by session
id bytes, written where `engine_session_for` inserts into
`negotiated_session_keys` and erased where `session_mark_invalidated` writes.
MEASURED target: `getIds()` count `1` after one handshake, `getSession(id)` the
SAME object, `getSession(new byte[0])` `null`, `getSession(null)` ->
`NullPointerException: session id cannot be null` (that last row is a defect on
its own today — the body answers `null` for a null id).

**N2 — `native-builtins/src/net_phase_e.rs:917` `ssc_get` / `:865` `SscSide`.**
Defaults are `0`/`0`; HotSpot measured `20480` (cache size) and `86400`
(timeout) on a brand-new `SSLContext`'s client *and* server contexts. `0` has a
defined meaning in this API — "unlimited" / "never expires" — so a caller that
reads the defaults back is told something specific and wrong. **Change:**
`impl Default for SscSide { fn default() -> Self { Self { cache_size: 20480,
timeout_secs: 86400 } } }`. Also make `ssc_bind` `pub(crate)` so
`t27_tls::getSessionContext`'s carrier can be bound instead of orphaned.

**N3 — `native-builtins/src/net_phase_e.rs:8296`-`8303`, the doc comment on
`register_https_session_accessors`.** Its shadowing analysis is wrong (§5.1);
five of its six bodies are dead. **Change:** replace the "none of them can be
overwritten by it" paragraph with the measured split, and note that
`getSSLSession` is the only live one. Better still (**N3b**): delete the five
dead bodies and the `https_carrier_session` table, move `getSSLSession` into
`http_url_connection.rs`'s registrar, and let one table serve all six.

**N4 — `docs/known-issues/jdk-only/INDEX.md`.** Not editable by this lane.
Three rows are stale: `C6-1` is marked OPEN with "the populator is a NOMINATION,
so every accessor answers 'not open'" — the populator LANDED (C12-2, and it is
present in the tree at `huc_verify_hostname` STEP 0), and the accessors' real
defect was the refusal TYPE, fixed here; `D3-3` should be **SUPERSEDED** by
E3-1 (re-raising E3-1's NOMINATION 1 — verified again here: `grep` over
`native-builtins/src` finds zero remaining raw `format!("{:?}", cs.suite())`
sites, and all seven producers go through
`t27_tls::suite_to_java_cipher_name`); and `C6-3` should be given the provenance
statement INDEX itself flags as missing — it is a **review heuristic, not a
measurement**, and its Witness A (`SimpleTimeZone`) is the only unfixed claim in
it.

**N5 — `native-builtins/src/servlet.rs`, `TlsEntry.raw`.** W7-61 item 2's
Windows half is still OPEN: the bounded-`WSAPoll` wake needs
`Option<TcpStream>` to become `Option<Arc<TcpStream>>`. The pilot named there
(`rustls_stream_read`'s client arm) is in `t27_tls.rs` and this lane could have
done it, but the type change it depends on is not. **Change:** widen the field,
then the pilot. Until then `tlsRead TIMEOUT` is expected.

**N6 — `regression-suite/run.sh:164` `CORE_CLASSES`.** F36-1 NOMINATION 1 is
still open: `RSslLiveSession` compiles and never runs. Two of this record's
findings (the non-null context on a negotiated session, the `getId` length
mid-handshake) have no scheduled vector until it does. This lane may not touch
`regression-suite/`.

**N7 — `native-builtins/src/tls.rs:1247` and `:3373`, the `--synthetic-jdk`
`getId` twins.** Neither carries `session_has_negotiated`'s width table, and
`:3373`'s `sun/security/ssl/SSLSessionImpl` copy is a separate slot. This lane
changed the real-mode gate; the synthetic copies now differ from it by one more
rule. **Change:** route both through `t27_tls::session_has_negotiated`, which is
already `pub(crate)` for exactly this reason.

---

## 9. Regression vector

Not created as a file — this lane may not write under `regression-suite/`.
`RSslNullSession` (89 checks, no network) and `RSslLiveSession` (95 checks,
loopback TLS) already exist; `RSslLiveSession` is not scheduled (N6). The rows
below are the ones this record's changes make falsifiable, and they belong in
`RSslLiveSession`'s existing `handshake` and `invalidate` families rather than
in a new class. Every expected value is MEASURED on the oracle (§1).

```java
    // --- G7: the session context is a state, not a constant -----------------
    // Requires a completed loopback handshake; `s` is client.getSession().
    static void g7Context(SSLSocket client, SSLSession s) throws Exception {
        ck("g7.ctx.negotiated.nonNull",
           String.valueOf(s.getSessionContext() != null), "true");
        ck("g7.ctx.negotiated.class",
           s.getSessionContext().getClass().getSimpleName().contains("SessionContext")
               ? "isContext" : s.getSessionContext().getClass().getName(),
           "isContext");

        // The null session is in no context. Same VM, same run.
        SSLSocket unconnected = (SSLSocket) SSLSocketFactory.getDefault().createSocket();
        ck("g7.ctx.nullSession", String.valueOf(
               unconnected.getSession().getSessionContext()), "null");
        unconnected.close();

        // ... and invalidate() takes a live one back out. This is the second
        // thing invalidate() moves; the first is isValid().
        ck("g7.ctx.beforeInvalidate", String.valueOf(s.getSessionContext() != null), "true");
        ck("g7.inv.isValid.before",   String.valueOf(s.isValid()), "true");
        byte[] idBefore = s.getId();
        s.invalidate();
        ck("g7.ctx.afterInvalidate",  String.valueOf(s.getSessionContext()), "null");
        ck("g7.inv.isValid.after",    String.valueOf(s.isValid()), "false");
        // invalidate() moves EXACTLY those two. Anti-vacuity for the pair:
        ck("g7.inv.id.survives",      String.valueOf(Arrays.equals(idBefore, s.getId())), "true");
        ck("g7.inv.id.len",           String.valueOf(s.getId().length), "32");
        ck("g7.inv.suite.survives",   s.getCipherSuite(), "TLS_AES_256_GCM_SHA384");
        ck("g7.inv.proto.survives",   s.getProtocol(), "TLSv1.3");
    }

    // --- G7: mid-handshake is a fully populated session ----------------------
    // Install this as the client's TrustManager. It is the ONLY way to observe
    // the mid-handshake window: getHandshakeSession() takes the same socketLock
    // the handshaking thread holds, so no other thread can read it, and a
    // HandshakeCompletedListener runs after the window has closed.
    // Rows are captured here and replayed through ck() on the main thread.
    static final Map<String,String> MID = new LinkedHashMap<>();

    static class G7Sampler extends X509ExtendedTrustManager {
        public void checkClientTrusted(X509Certificate[] c, String a) {}
        public void checkServerTrusted(X509Certificate[] c, String a) {}
        public void checkClientTrusted(X509Certificate[] c, String a, Socket s) {}
        public void checkClientTrusted(X509Certificate[] c, String a, SSLEngine e) {}
        public X509Certificate[] getAcceptedIssuers() { return new X509Certificate[0]; }

        public void checkServerTrusted(X509Certificate[] c, String a, Socket sk) {
            SSLSession hs = ((SSLSocket) sk).getHandshakeSession();
            MID.put("g7.mid.nonNull",  String.valueOf(hs != null));
            if (hs == null) return;
            MID.put("g7.mid.id.len",   String.valueOf(hs.getId().length));   // 32, NOT 0
            MID.put("g7.mid.suite",    hs.getCipherSuite());                 // real suite
            MID.put("g7.mid.proto",    hs.getProtocol());                    // TLSv1.3
            MID.put("g7.mid.isValid",  String.valueOf(hs.isValid()));        // true
            MID.put("g7.mid.ctx",      String.valueOf(hs.getSessionContext())); // null
            // The peer is not authenticated YET -- we are the code deciding it.
            try { hs.getPeerCertificates(); MID.put("g7.mid.peerCerts", "RETURNED"); }
            catch (SSLPeerUnverifiedException e) {
                MID.put("g7.mid.peerCerts", "SSLPeerUnverifiedException:" + e.getMessage());
            }
            MID.put("g7.mid.localCerts", String.valueOf(hs.getLocalCertificates()));
        }
    }

    static void g7MidHandshake() {
        // Anti-vacuity FIRST: an empty map means the callback never ran, and
        // every row below would otherwise pass by being absent.
        ck("g7.mid.sampled", String.valueOf(!MID.isEmpty()), "true");
        ck("g7.mid.nonNull",   MID.get("g7.mid.nonNull"),   "true");
        ck("g7.mid.id.len",    MID.get("g7.mid.id.len"),    "32");
        ck("g7.mid.proto",     MID.get("g7.mid.proto"),     "TLSv1.3");
        ck("g7.mid.isValid",   MID.get("g7.mid.isValid"),   "true");
        ck("g7.mid.ctx",       MID.get("g7.mid.ctx"),       "null");
        ck("g7.mid.peerCerts", MID.get("g7.mid.peerCerts"),
           "SSLPeerUnverifiedException:peer not authenticated");
        ck("g7.mid.localCerts",MID.get("g7.mid.localCerts"),"null");
        // Deliberately NOT asserted: that the mid-handshake id EQUALS the
        // completed session's id. It does on HotSpot; CratonVM caches the two
        // on different keys and cannot today. See G7-1 s4.
    }

    // --- G7: getHandshakeSession() is null on every reachable state ----------
    static void g7HandshakeSessionNulls(SSLSocket done) throws Exception {
        SSLSocket fresh = (SSLSocket) SSLSocketFactory.getDefault().createSocket();
        ck("g7.hs.unconnected",  String.valueOf(fresh.getHandshakeSession()), "null");
        fresh.getSession();
        ck("g7.hs.afterGetSession", String.valueOf(fresh.getHandshakeSession()), "null");
        fresh.close();
        ck("g7.hs.afterClose",   String.valueOf(fresh.getHandshakeSession()), "null");
        ck("g7.hs.postHandshake",String.valueOf(done.getHandshakeSession()),  "null");
    }

    // --- G7: HttpsURLConnection refuses with the right TYPE ------------------
    // No network: never connected, so no request is made.
    static void g7NotOpen() throws Exception {
        HttpsURLConnection h = (HttpsURLConnection)
            URI.create("https://127.0.0.1:1/").toURL().openConnection();
        // All six. getCipherSuite/getLocalCertificates/getLocalPrincipal have
        // NO throws clause, so SSLPeerUnverifiedException here is undeclared.
        ck("g7.notOpen.cipher",      kind(() -> h.getCipherSuite()),
           "IllegalStateException:connection not yet open");
        ck("g7.notOpen.serverCerts", kind(() -> h.getServerCertificates()),
           "IllegalStateException:connection not yet open");
        ck("g7.notOpen.localCerts",  kind(() -> h.getLocalCertificates()),
           "IllegalStateException:connection not yet open");
        ck("g7.notOpen.peerPrinc",   kind(() -> h.getPeerPrincipal()),
           "IllegalStateException:connection not yet open");
        ck("g7.notOpen.localPrinc",  kind(() -> h.getLocalPrincipal()),
           "IllegalStateException:connection not yet open");
        ck("g7.notOpen.sslSession",  kind(() -> h.getSSLSession()),
           "IllegalStateException:connection not yet open");
    }

    // --- G7: the buffer sizes MOVE when a suite is negotiated ----------------
    // Recorded, not asserted, if the 16384 under-report is left standing --
    // see t27_tls's getApplicationBufferSize comment. Assert only after that
    // constant is revisited.
    static void g7Buffers(SSLSession nullSess, SSLSession live) {
        ck("g7.buf.packet.null",   String.valueOf(nullSess.getPacketBufferSize()),      "16709");
        ck("g7.buf.packet.live",   String.valueOf(live.getPacketBufferSize()),          "16709");
        ck("g7.buf.app.null",      String.valueOf(nullSess.getApplicationBufferSize()), "16704");
        ck("g7.buf.app.live",      String.valueOf(live.getApplicationBufferSize()),     "16676");
    }
```

Anti-vacuity note, because this family has been bitten by it: `g7.mid.sampled`
must be asserted **before** the other `g7.mid.*` rows. F36-1 §1 records a probe
whose `HostnameVerifier` was never invoked and whose capture slot therefore
stayed null while every assertion "passed". The same failure mode applies to a
`TrustManager` callback: if the sampler never runs, `MID` is empty, and rows
read from it would compare `null` against `null` and be green for the wrong
reason.

---

## 10. What this lane did NOT do

- **It did not build, run, or measure CratonVM.** Not once. Every "after" in
  this record is source. The three files parse (`rustfmt --edition 2021
  --check`, empty stderr), carry zero CR bytes and zero conflict markers, and
  have no duplicate `fn` names — that is the whole of the verification.
- **It did not confirm the two shadowing findings against
  `--dump-native-registry`.** §5.1 and §5.2 are derived from reading the call
  orders in `lib.rs` and `phases_late.rs` and from comparing callback identity
  in a test. That is stronger than a comment and weaker than a dump. Listed in
  §7(5).
- **It did not make the mid-handshake id equal the completed id** (§4). The fix
  needs `engine_session_for`'s cache key to stop encoding `handshaked`, and that
  key is load-bearing for the sentinel values frozen into the pre-handshake
  object.
- **It did not touch `getApplicationBufferSize`.** The oracle says 16704/16676
  and this VM says a flat 16384; the existing comment argues that is a safe
  under-report against this VM's own engine and that raising it without auditing
  every `BUFFER_OVERFLOW` path is how a constant becomes an outage. That
  argument was not re-litigated without a binary. The measurement is recorded in
  §1 and a vector is drafted in §9, disabled.
- **It did not implement a real session cache.** `getSessionContext()` now
  answers a carrier; that carrier's `getIds()`/`getSession(id)` remain honest
  emptiness. N1.
- **It did not change `getHandshakeSession()` on either door.** Question 2's
  answer was "the merge is right", and acting on a question whose answer is "no
  change" would have been the worse outcome.
- **It did not fix the `record_https_peer_info` empty-chain early return**
  (§6), which makes one refusal arm reachable more often than the oracle would
  have it.
- **W7-63 and W7-71 were not advanced.** Both are FIXED-UNVERIFIED and both live
  entirely in files this lane does not own (`lib.rs`, `jca/*`, `crypto_impl.rs`,
  `classloading/`, `nio_file.rs`). Their open residuals are unchanged; nothing
  in this record bears on them.
- **W7-61 was only half-checked.** Item 1 is done and item 2's Windows half is
  still open; the blocking type change is in `servlet.rs`. N5.
- **D3-3 needed nothing.** Re-verified: zero raw `format!("{:?}", cs.suite())`
  sites remain in `native-builtins/src`, all producers route through
  `t27_tls::suite_to_java_cipher_name`, and the source-scanning test
  `the_only_rustls_suite_spelling_left_is_the_adapters_own` still asserts the
  idiom appears nowhere. **That test was not modified.** The record should be
  marked SUPERSEDED by E3-1 (N4).
