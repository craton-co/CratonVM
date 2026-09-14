# E31-1 — the unregistered door, the attribute slot that resurrects a fabrication, and the twins in the other mode

**Status: FIXED-UNVERIFIED (`native-builtins/src/t27_tls.rs`, `native-builtins/src/tls.rs`, this lane's files); NOMINATED (the rest).**
**Prov: HotSpot column MEAS (this host, `scratchpad/e31/E31HandshakeSessionSocket.java`, JDK 25.0.3+9-LTS); `jdk25src` citations READ; CratonVM column PRED.**
**2026-08-13, lane E31.** Lands NOMINATIONS A, D, E and G of
`E22-1-the-null-session-in-the-registrar-that-actually-answers.md`.

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

1. **`SSLSocket.getHandshakeSession()` is registered and the fixture is
   unblocked.** `RSslNullSession` runs **1 of its 47 checks** today — it dies on
   check 2 — and after this change all **47** execute. That is the measure of
   NOMINATION A, and it is larger than the 9 flips E22-1 predicted, because 46
   checks were not failing, they were not running. §1, §6.
2. **The attribute-slot collision is not inert and never was.** A single
   `putValue` on the unconnected socket's 3-field session overwrote
   `NEW13_SESS_TLSID`, which is the slot `session_has_negotiated` reads — so
   `isValid()` went back to `true` and `getId()` back to 32 fabricated bytes.
   **A `putValue` resurrected the exact fabrication E12/E22 removed.** The
   defensive `_ => true` arm stopped it becoming a *new* wrong answer; it was
   never a reason the collision was safe. Jetty's `SecureRequestCustomizer`
   calls `putValue` on every SSL request. §2.
3. **`invalidate()` is a fourth member of that family, and it inverts.**
   Nobody had named it. It wrote `SES_VALID` on any shape; on the 3-field
   shape that slot is the stream id, so `invalidate()` turned `Int(-1)`
   ("never connected") into `Int(0)` (a valid stream id) — **`isValid()` went
   from `false` to `true` because the caller asked to invalidate it.** §2.
4. **Everything E12 and E22 fixed is undone under `--synthetic-jdk`.**
   `tls.rs` wins there, and its copies of `isValid`, `getId`, `getCipherSuite`
   and `getProtocol` had all four defects those lanes removed — plus a
   `getCipherSuite` whose `_ => 0` default turned any `String` slot into
   `TLS_AES_256_GCM_SHA384`, i.e. reported a completed strong TLS 1.3
   handshake for a socket that was never connected. §4.
5. **`sun/security/ssl/SSLSessionImpl` is a drifted twin of every one of
   them**, ~1,900 lines later in the same file, on a layout its own `<init>`
   seeds identically. Its `getCipherSuite` was a hardcoded literal justified by
   a Keycloak heuristic that **does not exist**: Keycloak is checked out here
   (8,035 `.java` files) and contains zero `getCipherSuite` and zero
   `TLS_AES_128_GCM`. §4.
6. **The "no accessor exists" claim behind `"UNKNOWN"` was half wrong, and the
   two arms it covers are not one row.** The `LegacyDsa` arm is **openssl**,
   not native-tls, and `SslRef::version_str()` is unconditional — so the
   `"TLSv1.2"` sitting beside the suite was a fabrication with the real value
   one call away, on the *legacy* acceptor. §5.

## 1. The oracle — the SOCKET door of `getHandshakeSession()`

E12-1 measured this through the **engine** door. The socket door is a
different implementation and the question NOMINATION A turns on.

`scratchpad/e31/E31HandshakeSessionSocket.java`, HotSpot 25.0.3+9-LTS, loopback
with a self-signed PKCS12 from `keytool`. Three runs, one non-deterministic
line (noted below).

```
java.version = 25.0.3
ARM0 abstract-SSLSocket.getHandshakeSession    = THREW java.lang.UnsupportedOperationException
ARM0 abstract-SSLSocket.getApplicationProtocol = THREW java.lang.UnsupportedOperationException
ARMA class                                     = sun.security.ssl.SSLSocketImpl
ARMA unconnected.getHandshakeSession           = null
ARMA unconnected.getSession                    = SSLSessionImpl{cipher=SSL_NULL_WITH_NULL_NULL,
                                                 proto=NONE, id=0B, valid=false}
ARMA after getSession(), getHandshakeSession   = null
ARMA after close(), getHandshakeSession        = null
ARMM before startHandshake()                   = null
ARMM inside checkServerTrusted(chain,auth,Socket)
                                               = SSLSessionImpl{cipher=TLS_AES_256_GCM_SHA384,
                                                 proto=TLSv1.3, id=32B, valid=true}
ARMM inside HandshakeCompletedListener         = null
ARMF client after startHandshake() returned    = null
ARMF server after startHandshake() returned    = null
ARMF after close()                             = null
BUF null-session  app=16704 packet=16709
BUF handshaked    app=16676 packet=16709 suite=TLS_AES_256_GCM_SHA384
```

**ARM0 is the state CratonVM was in**, and it is measured rather than inferred
from source: an anonymous `javax.net.ssl.SSLSocket` subclass — one that does
not override the method — throws `UnsupportedOperationException`, exactly as
`jdk25src/java.base/javax/net/ssl/SSLSocket.java:474-476` reads. The same is
true of `getApplicationProtocol` (`SSLSocket.java:753`), which is why that one
already had a native in `t27_tls.rs` and this one being absent was easy to
miss: they are the same shape and only one had been noticed.

**The contract comes from the override, not the base class**
(`jdk25src/java.base/sun/security/ssl/SSLSocketImpl.java:384-392`):

```java
public SSLSession getHandshakeSession() {
    socketLock.lock();
    try {
        return conContext.handshakeContext == null ?
                null : conContext.handshakeContext.handshakeSession;
    } finally { socketLock.unlock(); }
}
```

### The mid-handshake state IS constructible, and that is the interesting part

The brief asked me to say so rather than guess if it were not. It is — but only
from **inside a handshake callback that is handed the `Socket`**, on the
handshaking thread. `X509ExtendedTrustManager.checkServerTrusted(chain,
authType, Socket)` is such a callback, and from there the session is fully
populated: suite, protocol, a 32-byte id, `valid=true`.

Three things that arm establishes and that no reading of the source would have:

* **It cannot be observed from another thread.** `getHandshakeSession()` takes
  the same `socketLock` `startHandshake()` holds. The callback works because
  `ReentrantLock` is re-entrant on the handshaking thread.
* **`HandshakeCompletedListener` is the wrong side of the window.** When it
  runs at all, it answers `null` — `handshakeContext` is already torn down.
  (This is the one non-deterministic line across the three runs: the client
  listener fires on its own thread and had not always run by the time main
  read it, printing `<never called>` in run 1 and `null` in runs 2-3. Both
  readings agree that it is never non-null.)
* **`getSession()` is not safe re-entrantly.** Calling it from inside the trust
  manager returned the *null session* and then broke the handshake outright —
  it initiates a handshake and blocks. Recorded because the first version of
  the probe did exactly that and produced a plausible, wrong transcript in
  which the connection appeared to complete with `SSL_NULL_WITH_NULL_NULL`.

### Why the registration answers `null` unconditionally

The non-null window is real and unreachable **in this VM**:
`engine_run_trust_check` (`t27_tls.rs`) dispatches exactly one descriptor,
`([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V` — the 2-arg
overload, which is handed no socket. There is no `SNIMatcher` callback either,
and every socket handshake here runs synchronously inside
`startHandshake()`/`createSocket(host, port)`. So no application code can hold
a CratonVM `SSLSocket` *and* be inside its handshake, and `null` is HotSpot's
answer for every state that is reachable. The condition is written into the
registration's doc comment rather than merely asserted, so that wiring up the
3-arg overload is a change that arrives with its own instruction.

**Where it is registered, and why that is not futile.** In `t27_tls.rs`'s
`register_t27_natives` (lib.rs ~18540, after `register_p68_ssl` at 18474), so
it is live in the default real-JDK mode and — since nothing else registers the
triple in either mode — under `--synthetic-jdk` too. The objection worth
answering is that `javax/net/ssl/SSLSocket` is a real loaded class **with** a
bytecode body, and `invoke.rs`'s hierarchy walk skips a native when the
receiver's own class declares the method. That skip is in the fallback arm;
`resolve_step1_native` matches the receiver's own class name first. The witness
is the fixture: DOOR 1's **first** check is `s.isConnected()`, served by a
native registered on this same class name, and E22-1 records the vector
reaching check 2 — so check 1 was answered by a native on a class whose
bytecode body exists. Same class, same mechanism.

## 2. NOMINATION G — the slot collision, and the two writers nobody counted

`num_fields - 1` is the attribute slot on **two of five** widths.

| width | minted by | `num_fields - 1` is | a `putValue` destroyed |
|---|---|---|---|
| 2 | `ssl_security`'s `SSLEngine.getSession` fallback | the cipher `String` | `getCipherSuite()` answers a `HashMap` |
| 3 | `ssl_security::new13_alloc_{,null_}ssl_session` | `NEW13_SESS_TLSID` | **the negotiation signal — see below** |
| 4 | `SSLServerSocket.accept` (`t27_tls`) | a dedicated attrs slot | nothing |
| 6 | `tls.rs::init_ssl_session_fields`; `http2.rs` | `SES_CREATION_TIME` | `getCreationTime()` returns an object reference through a `()J` descriptor |
| 8 | `build_synthetic_ssl_session` (`t27_tls`) | a dedicated attrs slot | nothing |

The 3-field row is the one that matters. Slot 2 there is the stream id **and**
the slot `session_has_negotiated` reads. So:

```
new SSLSocket (never connected)   -> session slot 2 = Int(-1) -> not negotiated
                                     getId() = byte[0], isValid() = false     [correct]
session.putValue("k", v)          -> session slot 2 = HashMap ref
                                     -> session_has_negotiated's `_ => true` arm
                                     -> getId() = 32 bytes, isValid() = true  [the fabrication, back]
```

E22-1 recorded the defensive arm as the reason "this change cannot turn the
corruption into a *new* wrong answer". That is true and it is a different claim
from "the corruption is harmless": the arm is what makes the resurrected answer
identical to the pre-E12 one rather than a third thing. **`sslsess_attrs_slot`
now returns `Option<usize>`** and `getValue`/`putValue`/`removeValue`/
`getValueNames`/`sslsess_attrs_map` all go through it; `putValue`/`removeValue`
no-op and `getValue`/`getValueNames` answer null/`String[0]` for a shape with
no slot — which is what those shapes already answer for a session nobody wrote
to. `sslsess_attrs_map` now takes the slot as a **parameter**, so the question
cannot be skipped by a future caller.

**The two writers E22-1's nomination did not count:**

* **`tls.rs`'s `invalidate()`** wrote `SES_VALID` (slot 2) on whatever shape it
  was handed. On the 3-field shape that is `Int(-1) -> Int(0)`, and `0 >= 0` is
  a *valid* stream id, so `invalidate()` made an unconnected socket's session
  **valid**. Exactly inverted, and free of any attribute API. Now gated on the
  shapes that have a flag slot.
* **`tls.rs`'s `getPeerHost`/`getPeerPort`/`getCreationTime`** read slots 3, 4
  and 5 with **no width test at all**. On the 4-field accept shape slot 3 is
  the attribute map, so `getPeerHost()` returned a `java.util.HashMap`
  reference through `()Ljava/lang/String;` once any caller had done a
  `putValue`; on the 2-, 3- and 4-field shapes slots 4 and 5 are off the end of
  the object. Now guarded, with HotSpot's measured null-session answers
  (`null`, `-1`, a real epoch) as the fallbacks.

**What becomes observably different.** For a session on a shape with no
attribute slot: `putValue` no longer changes `getId()`, `isValid()`,
`getCipherSuite()` or `getCreationTime()`; `getValue` after `putValue` returns
`null` where it previously returned the value only on the 4- and 8-field shapes
anyway. For the 4- and 8-field shapes: nothing changes. `invalidate()` on a
narrow shape is now a no-op instead of an inversion. That trade — a quiet miss
on a JSSE convenience API against a loud corruption of negotiation state — is
the same one E22-1's nomination proposed; widening the three shapes is the real
fix and stays nominated, because the 3-field one is `ssl_security.rs`'s and
cannot move without moving `session_has_negotiated`'s arms in the same commit.

**Also fixed, and it is the shape this directory keeps recording:** two
comments pointed at "`SSLSESS_ATTRS_SLOT`'s doc comment". **No such constant
has ever existed in this tree.** The rule they cited was `num_fields - 1`,
open-coded at five call sites. It exists now.

## 3. Task 4 — the width audit, including what was checked and cleared

Every session-shaped accessor in this lane's two files, against
`session_has_negotiated`.

### Cleared — checked, and correct as written

| accessor | file | what its width test decides | verdict |
|---|---|---|---|
| `getId` | `t27_tls` | calls `session_has_negotiated` | correct |
| `isValid` | `t27_tls` | calls `session_has_negotiated` | correct |
| `getCreationTime` / `getLastAccessedTime` | `t27_tls` | `> 5` = "is there a creation-time slot", **not** negotiation. Slot 5 is the creation time on both the 6- and 8-field shapes; narrower shapes answer `epoch_millis_now()`, and HotSpot returns a real epoch in *every* state including the null session | correct |
| `getPeerCertificates` | `t27_tls` | identity-keyed side table, no width inference | correct |
| `getApplicationBufferSize` / `getPacketBufferSize` | both | constants, no receiver read | correct value, **comment was wrong** — §5 |
| `SSLEngineResult` accessors (`<init>`, `getStatus`, `bytesConsumed`, `bytesProduced`) | `tls.rs` | width picks a *field convention*, not a session state | correct, and not session-shaped |
| `SSLEngineImpl.getHandshakeSession` | `t27_tls` | `is_handshaking()`, not a width | correct — and the model the socket door's comment points at |

### Changed — the same bug, in the mode this lane's other file owns

`register_tls_natives` is reached only from `register_synthetic_overrides`
(`#[cfg(feature = "synthetic-jdk")]`), so `tls.rs` is the live registrar under
`--synthetic-jdk` and shadows every `t27_tls` fix there.

| accessor | class | before | after (PRED) |
|---|---|---|---|
| `isValid` | `javax/net/ssl/SSLSession` | `num_fields > 5 ? slot2 : Int(1)` — **`true` for every shape narrower than 6**, i.e. the null session | `session_has_negotiated(..) && !invalidated` |
| `isValid` | `sun/security/ssl/SSLSessionImpl` | `num_fields > 2 ? slot2 : Int(1)` — returns the **stream id** through a `()Z`; `Int(-1)` on the null session, and `false` for a real accept session whose id is 0 | the same composition |
| `getId` | `javax/net/ssl/SSLSession` | 32 bytes unconditionally, of which **28 were always zero** (the fill loop ran `0..4`) | `byte[0]` unless negotiated; full 32-byte SplitMix fill |
| `getId` | `sun/security/ssl/SSLSessionImpl` | 32 bytes unconditionally, seeded from **`this.as_ptr()`** — the raw `ObjectRef`, the GC-unstable identity `gc_stable_objref_key` exists to replace | gated; `identity_hash_code` |
| `getCipherSuite` | `javax/net/ssl/SSLSession` | hard-coded slot 0 read as an `Int` index, `_ => 0`, and `TLS13_CIPHERS[0]` is **`TLS_AES_256_GCM_SHA384`** | width-correct slot; `String` first, then index, then the sentinel |
| `getProtocol` | `javax/net/ssl/SSLSession` | same shape, `_ => 0` = `"TLSv1.3"` | same treatment, `JSSE_NULL_PROTOCOL` |
| `getCipherSuite` / `getProtocol` | `sun/security/ssl/SSLSessionImpl` | **constants**, ignoring the receiver | read the session |
| `invalidate` | `javax/net/ssl/SSLSession` | wrote slot 2 on any shape | gated — §2 |
| `getPeerHost` / `getPeerPort` / `getCreationTime` | `javax/net/ssl/SSLSession` | slots 3/4/5, no width test | guarded — §2 |

The `getCipherSuite` row is the one to read twice. It is not merely a wrong
default: it is a **defaulting reader turning a wrong type into a confident
wrong answer**. Every shape except `tls.rs`'s own stores the negotiated names
as `String` references; the `Value::Int(i)` arm never matched them; the `_ => 0`
default is index 0; index 0 is the exact literal E12-1 §2 identifies as the
dangerous one, because it is in `getSupportedCipherSuites()` and therefore
indistinguishable from a real negotiation by any test a caller can write. So
under `--synthetic-jdk`, an unconnected socket whose producer had just been
fixed to write `SSL_NULL_WITH_NULL_NULL` was reported as a completed TLS 1.3
handshake on the strongest suite in the list.

### The slot-order rule was also false, and harmlessly so

`>= 7` put the 6-field shape on the wrong side: `tls.rs`'s and `http2.rs`'s
sessions are **cipher-first**, like the 8-field engine shape, not
protocol-first like the narrow ones. It was inert only because that shape
stores `Int` indices, so both accessors fell to the sentinel rather than
returning each other's value. The rule is now `>= 6`, lives once
(`session_cipher_slot`/`session_proto_slot`), returns `Option` so the
"too short to carry the pair" case cannot be indexed past the end of a
1-field receiver, and is pinned by a unit test. No answer changes today.

## 4. `sun/security/ssl/SSLSessionImpl` — a twin whose divergence had no reason

Four accessors, ~1,900 lines below their `javax/net/ssl/SSLSession` siblings in
the same file, on a layout that this class's own `<init>` seeds with the same
`init_ssl_session_fields`. The registrar's header comment listed the four
defects — *"getId derived from pointer, cipher/protocol hardcoded,
isValid()=true"* — as an inventory, written as though listing them retired
them.

The `getCipherSuite` literal is the one with a checkable justification, and it
does not survive the check. The comment said `TLS_AES_128_GCM_SHA256` was
"picked over `TLS_AES_256_GCM_SHA384` so Keycloak's 'is this a modern suite?'
heuristic (name starts with `TLS_AES_128_GCM_`) passes". **Keycloak is checked
out on this host** — `C:\craton\apps\keycloak`, 8,035 `.java` files — and a
full `grep -rl` over it finds **zero** occurrences of `getCipherSuite` and
**zero** of `TLS_AES_128_GCM`. The heuristic is not there.

Which is also why nobody noticed the literal **contradicts its sibling**: for
one and the same object, `javax/net/ssl/SSLSession.getCipherSuite()` answered
`TLS13_CIPHERS[0]` = `TLS_AES_256_GCM_SHA384` while
`SSLSessionImpl.getCipherSuite()` answered `TLS_AES_128_GCM_SHA256`. Two doors,
one session, two suites. Reading the state makes them agree; the door that
changes is `SSLSessionImpl`'s, from `..._128_...` to `..._256_...`, and the
measured denominator for that change is 0.

## 5. NOMINATION D and E

**D — the buffer-size comment in `tls.rs`.** Corrected, values unchanged. The
old text asserted 16384/16709 were "the same pair a stock JDK returns" and that
"real `SSLSessionImpl` varies these only via `SSLParameters
.setMaximumPacketSize` and only for DTLS". Both clauses are false. Measured
here: **16704** on the null session, **16676** after a TLS 1.3 handshake, never
16384. Derived from source rather than only measured:
`SSLSessionImpl.java:1297` returns `SSLRecord.maxRecordSize -
SSLRecord.headerSize` with `headerSize = 5` (`SSLRecord.java:35`) = 16709 - 5 =
16704, and once a suite is negotiated it returns
`cipherSuite.calculateFragSize(...)` (`CipherSuite.java:1051-1076`), which
subtracts the header and then the AEAD tag and explicit-IV remainder — which is
*why* the real answer varies with the suite and cannot be a constant.
`SSLRecord.maxDataSize` (= 16384), the constant the old text named, is reached
by this method only on the DTLS branch. The value stays 16384 deliberately: a
safe under-report against this VM's own engine, and raising it without auditing
every `BUFFER_OVERFLOW` path is how a constant becomes an outage. What was
wrong was the assertion.

**E — naming the native-tls suite.** `"UNKNOWN"` is now
`NATIVE_TLS_UNNAMEABLE_SUITE`, defined once, carrying the argument for why it
must **not** become `SSL_NULL_WITH_NULL_NULL`.

**I did not touch that judgement and I agree with it**, so I will state the
reasoning I am *not* arguing against: those two arms are reached **by
succeeding** — `accept()` returned, the stream is encrypted, a suite genuinely
was negotiated — and writing "no cipher" about a live encrypted connection is
false in the *dangerous* direction, the same direction as the fabrication this
family removed, merely inverted. `"UNKNOWN"` is loud rather than plausible.
That is the correct trade and the constant exists to keep it from being
"completed" by a future family fix.

What the check *did* change is the claim about availability, in both
directions:

* E22-1 offered "read the suite out of the underlying `SslStream` via the
  backend-specific escape hatch" as a way out. **For the `Native` arm there is
  no hatch.** `native-tls` 0.2.18's `TlsStream` exposes exactly
  `buffered_read_size`, `peer_certificate`, `tls_server_end_point`,
  `negotiated_alpn`, `shutdown`; `get_ref`/`get_mut` return the underlying
  transport, not the backend handle; `imp::TlsStream` is private. Even
  `negotiated_alpn` is `#[cfg(feature = "alpn")]` and this workspace takes
  `native-tls = "0.2"` with default features (`default = []`), so it does not
  exist in this build — reading it would not compile. (I wrote that call and
  reverted it. Recorded because "the accessor exists" is the obvious next move
  and it is wrong.)
* **The two arms are not one row.** `LegacyDsa` is
  `openssl::ssl::SslStream`, not native-tls. `SslRef::version_str()`
  (`SSL_get_version`) is **unconditional** in openssl 0.10.76 — no `ossl111`
  gate — so the `"TLSv1.2"` sitting beside the suite was a *fabrication with
  the real value one call away*, and on the **legacy** acceptor of all places:
  a caller testing "am I on at least TLS 1.2" was told yes for a connection
  that may have been 1.0 or 1.1. False in the dangerous direction, by the same
  argument the cipher comment makes. It now reads the real version.
  Its cipher stays unnamed because the JSSE-spelled accessor,
  `SslCipherRef::standard_name()`, **is** `#[cfg(ossl111)]`, and
  `name()` — which is unconditional — returns the OpenSSL spelling
  (`ECDHE-RSA-AES256-GCM-SHA384`). Handing JSSE callers an OpenSSL-vocabulary
  name is a different wrong answer, not a right one.

The `Native` arm's `"TLSv1.2"` is **unchanged and disclosed rather than
fixed**: native-tls exposes no version accessor either, and replacing a
plausible value with a loud one there is a behaviour change on a live,
succeeding connection that this lane cannot run. NOMINATION 3 below.

## 6. The fixture — what becomes REACHABLE

`regression-suite/src/RSslNullSession.java`, 47 checks, PASS on HotSpot. Not
edited (not this lane's file).

Structure: DOOR 1 = `isConnected` + `getHandshakeSession` + `nullSession(13)` +
2 × `getEnabledProtocols` + `nullSession(13)` = 30; DOOR 2 =
`getHandshakeSession` + `nullSession(13)` = 14; `unofferable` = 3. Total 47.

**Today: 1 of 47 checks executes, and it is RED.** Line 146,
`ck("socket.getHandshakeSession", String.valueOf(s.getHandshakeSession()),
"null")`, throws `UnsupportedOperationException` out of `main`, so the
remaining 46 never run. The fixture reports nothing about either prior lane's
work — which is the point E22-1 made and the reason its 9 predicted flips could
not be observed.

The one check that *does* run is `socket.isConnected`, and reading its native
rather than assuming it passes turns up a divergence nobody has recorded:
`ssl_security.rs:4167` answers `isConnected()` as **"not closed"**, and a
freshly minted, never-connected socket is not closed — so it returns `true`
where the fixture (and HotSpot) want `false`. A socket that has never been
given a host is not connected; "closed" and "connected" are different states
and this conflates them. **PRED RED, before and after this change**, and
invisible until now because the vector aborts on the next line and never
prints a summary. NOMINATION 6.

**After this change: all 47 execute.** The 46 newly-reachable checks, by group:

| group | count | newly reachable | PRED |
|---|---|---|---|
| `socket.isConnected` | 1 | no — already running | **RED**, and pre-existing: NOMINATION 6 |
| `socket.getHandshakeSession` | 1 | yes | **`null`** — the registration |
| `socket.*` null-session block | 13 | yes | green (E12 edit 1 + E22 edits 1-4, all live in real-JDK mode) |
| `socket.getEnabledProtocols` × 2 | 2 | yes | green (E12 site 8) |
| `socketClosed.*` null-session block | 13 | yes | green — validity is read off the session's own recorded id, not stream liveness, so `close()` changes nothing |
| `engine.getHandshakeSession` | 1 | yes | **`null`** (E22 edit 5) |
| `engine.*` null-session block | 13 | yes | green (E22 edits 1-4) |
| `unofferable.*` | 3 | yes | green |

So the honest statement of this lane's effect on the fixture is **not** "9
checks flip"; it is "46 checks start running, and the 9 E22-1 named are among
them". A red among the other 37 is a finding, not a regression — none of them
has ever executed on this VM.

Nothing in this lane changes a DOOR-1 or DOOR-2 answer in real-JDK mode: the
attribute-slot and `session_*_slot` changes are no-ops for the 3- and 8-field
shapes the fixture exercises (`getValueNames` answered `String[0]` before via a
missed `Int` read and answers it now via `None`; the slot order for widths 3
and 8 is unchanged by the `>= 7` -> `>= 6` correction). The `tls.rs` changes
are `--synthetic-jdk`-only. That is deliberate: this lane's job on the fixture
was to make it *run*.

## 7. Residuals

1. **Nothing here was built or run.** No `cargo`, no VM, no fixture. The Rust
   was reviewed by hand against the existing shapes in the same files, and both
   files pass a bracket/string/comment-aware structural scan. One borrow hazard
   was found and fixed by inspection: `match session_cipher_slot(..).map(|slot|
   ctx.get_field(this, slot)) { .. }` puts a closure holding `&ctx` in the
   match scrutinee, where temporaries live to the end of the match, colliding
   with the `&mut ctx` the sentinel arms need. All six sites bind to a `let`
   first, and each carries a comment saying why.
2. **`tls.rs` is CRLF and `t27_tls.rs` is LF.** Both preserved; verified by
   byte count after every edit. One batch edit to `tls.rs` was attempted with a
   script, failed to match, and was redone through the editor for this reason.
3. **`isValid` and `getId` are deliberately NOT one predicate in `tls.rs`.**
   E22-1 established that they are one predicate in HotSpot, and that is true of
   `isRejoinable()` — but `isRejoinable()` reads `sessionId.length() != 0` **and**
   `invalidated`, two fields, and `getId()` reads neither. Measured (E12-1 §1
   arm E): `invalidate()` changes `isValid()` and nothing else. So `getId` is
   gated on negotiation alone and `isValid` on negotiation *and* the valid
   flag. Collapsing them would have made `invalidate()` erase the id — a new
   divergence traded for an old one.
4. **A `putValue` on a 2-, 3- or 6-field session no longer round-trips.** It
   never round-tripped correctly; it corrupted a field instead. The quiet miss
   is the deliberate trade (§2), and widening those shapes is NOMINATION 1.
5. **`http2.rs`'s 6-field session is still a fifth width** and is minted with
   no slot written. It is unaffected by everything here (every accessor answers
   the sentinel or a real epoch for it), but it is the shape that keeps the
   width count at five.
6. **`putValue`'s null-argument failure is an NPE, not `IllegalArgumentException`.**
   Pre-existing, in `t27_tls`, untouched. Flagged because it sits three lines
   from a hunk in this change and should not be bisected onto it.
7. **`SSLSessionImpl.getCipherSuite`'s answer changes** from
   `TLS_AES_128_GCM_SHA256` to whatever the session holds (for `tls.rs`'s own
   shape, `TLS_AES_256_GCM_SHA384`). Denominator measured at 0 in Keycloak
   (§4), but Keycloak is the only corpus I grepped for this specific consumer.

## How to verify

Cheapest first. Every CratonVM row is PREDICTED.

1. **`cargo test -p cratonvm-native-builtins t27_tls`** — three new unit tests,
   no VM, no network.
   `a_put_value_cannot_resurrect_the_null_sessions_id_or_validity` is the
   collision; `a_shape_with_a_real_attribute_slot_still_round_trips` is its
   **mutation check** — without it, `sslsess_attrs_slot` could return `None`
   for every shape and the first test would still pass, which is the
   "measured the refusal and called it coverage" shape.
   **Run the mutation check the mutation way**: make `sslsess_attrs_slot`
   return `None` unconditionally and the second test must fail while the first
   still passes. `the_session_slot_conventions_are_split_at_six_fields_not_seven`
   pins the width table.
2. **`--dump-native-registry`** (flags BEFORE `-cp`) — confirm
   `javax/net/ssl/SSLSocket.getHandshakeSession` now appears, with
   `owns_slot: true`, and that nothing later takes the slot back.
3. **`bash regression-suite/run.sh` with `ONLY="RSslNullSession"`** — the real
   measure. It should now print a `checks=` line at all. §6 says which 46
   checks are running for the first time; treat any red among them as new
   information rather than as a regression.
4. **`cargo test -p cratonvm-native-builtins --features synthetic-jdk tls`** —
   `tls.rs`'s changes are invisible to the default build. Its existing tests
   are registration-presence assertions only, so a green run there proves the
   registrations still exist and nothing about the new bodies; item 1's tests
   cover the shared predicates those bodies now call.
5. **Any embedded-HTTPS fixture, and any legacy/DSA TLS fixture.** The first
   should be unchanged — every session edit is on a path reached only when
   nothing was negotiated. The second exercises the one deliberate behaviour
   change on a *succeeding* path: `SSLSession.getProtocol()` for a
   `LegacyDsa`-accepted socket now reports the real version instead of
   `"TLSv1.2"`. If it was genuinely TLS 1.2 the answer does not move.

---

## NOMINATION 1 — `ssl_security.rs` / `http2.rs`: widen the three shapes that have no attribute slot

The real fix behind §2. `sslsess_attrs_slot` currently answers `None` for
widths 2, 3 and 6, which makes `putValue` a no-op there. Giving those shapes a
dedicated last slot makes the attribute API work everywhere and removes the
`None` arm.

**Ordering caution, and it is the same one E22-1's NOMINATION C carries:**
widening the 3-field `NEW13_SSL_SESS_FIELDS` moves it through
`t27_tls::session_has_negotiated`'s arms. At 3 the arm tests `slot2 >= 0`; at 4
it falls into `_ => true` and **the null session becomes valid again**; at 7+ it
tests `slot2 != 0`, which is a different meaning of slot 2 entirely. Widen to a
size whose arm you have read, in the same commit as the arm.

For `http2.rs`'s 6-field session the caution is milder but real: it is minted
with **no slot written**, so widening it past 6 moves it from
`session_has_negotiated`'s `_ => true` arm to the `slot2 != 0` arm, reading a
slot nobody set. Widen *and* initialise, or leave it.

## NOMINATION 2 — `ssl_security.rs`: `new13_alloc_ssl_session`'s shape should carry its own attribute slot before anything else widens

Narrower than NOMINATION 1 and the one with a named caller: Jetty's
`SecureRequestCustomizer.retrieveSni()` does `getValue()` then `putValue()` on
**every** SSL request, and the 3-field shape is what
`new13_resolve_socket_session` hands it. Today that `putValue` is a no-op (this
change) where it used to be a corruption (before it). Neither is what Jetty
asked for.

## NOMINATION 3 — `t27_tls.rs` (this lane's file, deliberately deferred): the `Native` acceptor's protocol version

§5. The `LegacyDsa` arm now reports its real protocol version because openssl
exposes one. The `native_tls::TlsAcceptor` arm still answers a hardcoded
`"TLSv1.2"` for a handshake whose version it cannot read, and that is the same
species of fabrication the sibling comment refuses for the cipher — a real,
plausible value asserted about an unknown outcome, on an acceptor that exists
to serve *legacy* peers.

Not changed here because it is a behaviour change on a live, succeeding
connection and this lane cannot run one. Two ways out, in the order I would
take them: (a) route this path through rustls, which already reports both, and
retire the arm; (b) answer `NATIVE_TLS_UNNAMEABLE_SUITE`'s protocol twin — a
loud value — and measure what breaks. Option (b) needs a `startsWith("TLS")`
consumer census first; option (a) is a behaviour change for the fixtures the
legacy acceptor exists for.

## NOMINATION 4 — `ssl_security.rs`: `getPeerPrincipal` reads slot 2 as a stream id (re-raising E22-1's NOMINATION B, now with a second reason)

E22-1 nominated this because on the 8-field shape slot 2 is the `isValid` flag.
There is now a second, sharper reason: with `putValue` no longer able to
corrupt slot 2, that slot is a *reliable* signal on every shape — and this
accessor still reads it with `> NEW13_SESS_TLSID`, i.e. on any shape with 3+
fields, so it queries `s2_tls_peer_cert_chain_der(0)` or `(1)` for an engine
session. Cross-talk into a different shape's socket registry. The fix is
E22-1's: match the exact width.

## NOMINATION 6 — `ssl_security.rs`: `SSLSocket.isConnected()` conflates "not closed" with "connected"

The single check `RSslNullSession` currently reaches, and it is red.

```rust
// native-builtins/src/phases_late/ssl_security.rs:4167
r.register(ssl_sock, "isConnected", "()Z", |ctx, args| {
    let this = obj_arg(args, 0)?;
    let closed = match ctx.get_field(this, NEW13_SOCK_CLOSED) { .. };
    Ok(Some(Value::Int(if closed { 0 } else { 1 })))
});
```

A socket from the zero-arg `createSocket()` has never been given a host, has
`NEW13_SOCK_TLSID = -1`, and is not closed — so this answers `true`. HotSpot
answers `false` (`java.net.Socket.isConnected()` is "has this socket ever been
successfully connected", a latch set by `connect()`, not the negation of
`isClosed()`). The two states are independent: HotSpot's arm G in E12-1 is a
socket that is both **closed** and **connected**.

The fix is a connected latch, not a different reading of the closed flag —
`NEW13_SOCK_TLSID >= 0` is already exactly that signal on this shape and is
what `new13_finish_socket` sets. Note the sibling `isClosed()` two
registrations down is correct and must not change with it.

Recorded here rather than fixed because `ssl_security.rs` is not this lane's
file, and because it should land *after* the `getHandshakeSession`
registration: until that lands, nothing downstream of it runs, so this
divergence cannot be observed as anything but an abort.

## NOMINATION 7 — `docs/known-issues/jdk-only/INDEX.md`: list this record

New `.md` files under this directory are this lane's to create; `INDEX.md` is
not. Add
`E31-1-the-unregistered-door-and-the-slot-that-resurrects-a-fabrication.md`
alongside `E22-1` and `E12-1`, which it continues.

## NOMINATION 5 — a census of `object_num_fields` on session-shaped receivers outside these two files

This lane found five width-blind or width-blind-equivalent session accessors in
two files, three of which nobody had named. The generalisable rule is the one
`session_cipher_slot` now documents: **a field count answers "which layout is
this", never "what happened to this connection"**, and the two questions had
been conflated at every site. `ssl_security.rs`, `http2.rs` and `net_phase_e.rs`
all read session shapes and were not audited here.
