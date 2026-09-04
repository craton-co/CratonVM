# E12-1 — the session that never handshaked: one fabrication, one crash, and eleven accessors that are not uniform

**Status: FIXED-UNVERIFIED (this lane's file); NOMINATED (the rest).**
**Prov: HotSpot column MEAS (this host); CratonVM column PRED.**
**2026-08-13, lane E12.** Closes residual 1 and NOMINATION 2 of
`E3-1-the-cipher-name-helper-and-its-real-denominator.md`.

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
**PREDICTED**. The JSSE numbers are executed on this host, HotSpot
25.0.3+9-LTS, by four probes in `scratchpad/e12/`:

| probe | what it establishes |
|---|---|
| `E12SessionContract.java` | all 15 accessors × 8 states (§1) |
| `E12HandshakeSession.java` | `getHandshakeSession()` over a live handshake; **unofferability** (§2) |
| `E12TwoDoors.java` | the `HttpsURLConnection` door beside the `SSLSocket` door (§3) |
| `E12Enabled.java` | `getEnabledProtocols()` on the same unconnected socket (§5) |

The Rust shapes this lane introduced were type-checked and executed standalone
with plain `rustc` (`scratchpad/e12/tycheck.rs`). The new fixture was compiled
and run on HotSpot: **47 checks, byte-identical over 3 runs**.

---

## 0. Verdict

E3-1 flagged the fallbacks as "all wrong". They are, but the shape is worse and
more interesting than a bad literal:

1. **`SSLSocket.getSession()` returned `null`** for an unconnected socket — a
   `NullPointerException` in the caller where HotSpot has a defined answer.
   The doc comment sitting on that `return` **already described the correct
   behaviour** ("the one case where JSSE itself would hand back an invalid
   session rather than a real one") and then did the opposite. This is the
   worst of the four, and no prior record in this family noticed it.
2. **`build_synthetic_ssl_session` fabricates `TLS_AES_256_GCM_SHA384`** — a
   real, strong, *supported* suite name for a session that never negotiated
   one. §2 explains why "supported" is the word that makes it dangerous.
3. **`getId()` fabricates 32 bytes** where JSSE answers `byte[0]`. This is the
   second fabrication and it has a **named, measured consumer** (§4).
4. `"UNKNOWN"`, `"?"`, `"TLS"`, `"TLSv1.3"` — wrong but distinguishable.

**The accessors are NOT uniform, and any fix that treats them as one family is
wrong.** For the same "nothing negotiated" state, JSSE returns a sentinel from
two of them, an *empty* value from three, `null` from two, a real number from
three, and **throws** from two. §1 is the table.

## 1. The oracle, measured — 15 accessors × the states that matter

`scratchpad/e12/E12SessionContract.java`, HotSpot 25.0.3+9-LTS. Arms:
**A** fresh unconnected `SSLSocket` → `getSession()`; **B** `SSLEngine` before
`beginHandshake()`; **D** `SSLEngine` after a completed in-memory TLS 1.3
handshake; **E** D's session after `invalidate()`; **F** `SSLSocket` over
loopback after `startHandshake()`; **G** F's session after the socket is
closed; **H** G's session after `invalidate()`.

| accessor | A unconnected | B pre-handshake | D handshaked | E after invalidate | G after close |
|---|---|---|---|---|---|
| `getCipherSuite()` | `SSL_NULL_WITH_NULL_NULL` | `SSL_NULL_WITH_NULL_NULL` | `TLS_AES_256_GCM_SHA384` | **unchanged** | unchanged |
| `getProtocol()` | `NONE` | `NONE` | `TLSv1.3` | **unchanged** | unchanged |
| `getId()` | `byte[0]` | `byte[0]` | `byte[32]` | **unchanged** | unchanged |
| `isValid()` | `false` | `false` | `true` | **`false`** | **`true`** |
| `getCreationTime()` | real epoch ms | real epoch ms | real epoch ms | unchanged | unchanged |
| `getLastAccessedTime()` | real epoch ms | real epoch ms | real epoch ms | unchanged | unchanged |
| `getApplicationBufferSize()` | **16704** | **16704** | **16676** | unchanged | unchanged |
| `getPacketBufferSize()` | 16709 | 16709 | 16709 | unchanged | unchanged |
| `getPeerHost()` | `null` | `null` | the requested host | unchanged | unchanged |
| `getPeerPort()` | `-1` | `-1` | the requested port | unchanged | unchanged |
| `getPeerCertificates()` | **THROWS** `SSLPeerUnverifiedException` | **THROWS** | the chain | **the chain** | the chain |
| `getPeerPrincipal()` | **THROWS** `SSLPeerUnverifiedException` | **THROWS** | the subject | **the subject** | the subject |
| `getLocalCertificates()` | `null` | `null` | `null` (no client auth) | unchanged | unchanged |
| `getLocalPrincipal()` | `null` | `null` | `null` (no client auth) | unchanged | unchanged |
| `getValueNames()` | `String[0]` | `String[0]` | `String[0]` | unchanged | unchanged |

Three results here are load-bearing and none of them were guessable:

* **`invalidate()` changes exactly one thing: `isValid()`.** Cipher, protocol,
  id and certificates all survive. A design that made an invalidated session
  answer the sentinel would be as wrong as the fabrication it replaced. The
  brief asked whether an invalidated session should answer the sentinel; **the
  oracle says no, emphatically.**
* **Closing the socket does NOT invalidate the session** (arm G: `isValid()`
  is still `true`). The session outlives its transport, for resumption.
  CratonVM's `isValid` ties validity to a live stream and answers `false` —
  §7 residual 2.
* **`getSession()` on an unconnected socket is not idempotent-by-identity**:
  DOOR 3 of `E12TwoDoors` shows two calls returning two *different*
  `SSLSessionImpl` instances. So the null session must not be cached.

### `getHandshakeSession()` is a third answer again

`scratchpad/e12/E12HandshakeSession.java`, stepping a client/server engine pair:

```
step  cHsStatus         c.getHandshakeSession()                 s.getHandshakeSession()
   0  NEED_WRAP         null                                    null
   2  NEED_UNWRAP       null                                    TLS_AES_256_GCM_SHA384/TLSv1.3 id=32B
   5  NEED_WRAP         TLS_AES_256_GCM_SHA384/TLSv1.3 id=32B   TLS_AES_256_GCM_SHA384/TLSv1.3 id=32B
   9  NEED_WRAP         null                                    TLS_AES_256_GCM_SHA384/TLSv1.3 id=32B
  10  NOT_HANDSHAKING   null                                    null

steps where client getHandshakeSession() was non-null = 4
after completion, c.getHandshakeSession() = null
```

**Null before, non-null and fully populated during, null after.** CratonVM
answers a populated synthetic session unconditionally (`t27_tls.rs:10055`), so
it is right only in the middle window and wrong on both sides — including for
the exact caller its own comment names (Jetty's `SslConnection.getBufferSize()`
on a brand-new connection, i.e. *before* the handshake).

### The one measured divergence this lane is NOT fixing

`getApplicationBufferSize()` is **16704** on the null session and **16676**
after a TLS 1.3 handshake — never 16384. Both CratonVM copies
(`t27_tls.rs:11612`, `tls.rs`) return a hardcoded `16384` under a comment that
says *"KEEP (correct constants — these are protocol limits, not placeholders).
The real JDK returns 16384 (`SSLRecord.maxDataSize`…)"*. **That comment is
measurably wrong**, and it is wrong in the confident register that stops the
next reader from checking. `getPacketBufferSize` = 16709 is correct in every
state. The under-report is self-consistent (CratonVM's own engine never
produces more than it advertises), so this is a documentation defect with a
latent behavioural edge, not a live bug — NOMINATION 6.

## 2. THE CRUX — sentinel or refusal, and why this is not a fabrication

`--jdk-only` exists to refuse invented stand-ins rather than serve them. So the
question is not rhetorical: is answering `SSL_NULL_WITH_NULL_NULL` an invention?

**No, and the discriminating property is measurable rather than aesthetic.**

> A fabricated stand-in is a value **the caller cannot tell apart from a real
> one**. A sentinel is a value the caller **can always tell apart**, because the
> platform guarantees it can never be produced by success.

`E12HandshakeSession.java` measures exactly that guarantee:

```
SSL_NULL_WITH_NULL_NULL in getSupportedCipherSuites()  = false
setEnabledCipherSuites("SSL_NULL_WITH_NULL_NULL")      THREW java.lang.IllegalArgumentException:
                                                             Unsupported CipherSuite: SSL_NULL_WITH_NULL_NULL
TLS_AES_256_GCM_SHA384  in getSupportedCipherSuites()  = true
TLS_AES_128_GCM_SHA256  in getSupportedCipherSuites()  = true
"NONE" in getSupportedSSLParameters().getProtocols()   = false
"TLS"  in getSupportedSSLParameters().getProtocols()   = false
supported protocols = [TLSv1.3, TLSv1.2, TLSv1.1, TLSv1, SSLv3, SSLv2Hello]
```

**`SSL_NULL_WITH_NULL_NULL` is unofferable.** JSSE refuses to *enable* it, so
no handshake can ever end with it, so returning it cannot be mistaken for a
handshake. It is IANA `0x0000` — a real registered name whose registered
meaning is "no cipher". Returning it is reporting "nothing negotiated" in the
caller's own vocabulary. That is honesty.

`TLS_AES_256_GCM_SHA384` has the exact opposite property: it is in the
supported list and it is byte-for-byte what a real TLS 1.3 handshake produces
on both VMs. There is **no test a caller can write** that separates CratonVM's
guess from a genuine negotiation. That is invention, and it is why a fabricated
cipher name is worse than a merely wrong one — **security-sensitive
application code branches on this string.** Code that asks "am I on a strong
suite before I send this" gets `yes` from a session that has never handshaked.
`"UNKNOWN"` at least fails that test loudly; `TLS_AES_256_GCM_SHA384` passes it
falsely.

So the rule this lane applies, and the one the sentinel earns:

> **Answer the sentinel where JSSE hands out a session object. Refuse where
> JSSE cannot produce one at all. Never invent a value that lives in the same
> namespace as a successful answer.**

`"NONE"` is the weaker half and the record should say so: it is not in the
supported protocol list, but `setEnabledProtocols("NONE")` was **accepted**
without complaint, so its unofferability rests on "it is not a protocol" rather
than on an enforced refusal. It is still the JDK's own literal for this state,
and still strictly better than `"TLS"` — which is not a protocol either *and*
is the standard `SSLContext.getInstance` **algorithm** name, so it reads as
legitimate.

## 3. (d) THE TWO DOORS — the consistency check, re-measured rather than cited

C6-1 recorded that all six `HttpsURLConnection` session accessors throw
`IllegalStateException: connection not yet open` for an unconnected connection.
This lane re-ran that on the same JVM, in the same process, beside the
`SSLSocket` door, so the rule rests on one transcript
(`scratchpad/e12/E12TwoDoors.java`):

```
=== DOOR 1: HttpsURLConnection, never connected ===
  class = sun.net.www.protocol.https.HttpsURLConnectionImpl
  getCipherSuite         THREW java.lang.IllegalStateException: connection not yet open
  getServerCertificates  THREW java.lang.IllegalStateException: connection not yet open
  getLocalCertificates   THREW java.lang.IllegalStateException: connection not yet open
  getPeerPrincipal       THREW java.lang.IllegalStateException: connection not yet open
  getLocalPrincipal      THREW java.lang.IllegalStateException: connection not yet open
  getSSLSession          THREW java.lang.IllegalStateException: connection not yet open

=== DOOR 2: SSLSocket, never connected -> getSession() ===
  getSession()      = sun.security.ssl.SSLSessionImpl
  getCipherSuite()  = SSL_NULL_WITH_NULL_NULL
  getProtocol()     = NONE
  isValid()         = false
  getId().length    = 0
```

**C6-1's transcript reproduces exactly, and the asymmetry is HotSpot's own.**
The same underlying state — nothing has handshaked — answers a sentinel through
one door and refuses through the other, on one VM, in one run. So reproducing
both is not an inconsistency to be smoothed over; smoothing it would be the
divergence.

The rule that makes them consistent is the one in §2, and it is about *whether a
session object exists*:

* `HttpsURLConnection.getCipherSuite()` asks "what did **this connection**
  negotiate". There is no connection and no session — nothing to ask. It
  refuses, and `net_phase_e::register_https_session_accessors` already refuses
  with the same exception and message. **Unchanged by this lane, and correct.**
* `SSLSession.getCipherSuite()` asks "what did **this session** negotiate", and
  a session object demonstrably exists — JSSE hands you `nullSession` rather
  than null. It answers.

The failure mode to avoid is answering the sentinel where HotSpot refuses, or
refusing where HotSpot has a session. **CratonVM was doing the second, in the
worst form: it returned null, which is neither.** Verified for the HTTPS door
specifically: `net_phase_e`'s accessors reach a session object only via
`https_carrier_session`, which is populated after a handshake, so none of this
lane's fallbacks can leak into that path.

## 4. THE DENOMINATOR — who actually branches on these values

The brief asked for the true consumer count rather than an assertion that there
are none. Grepping the shape across in-tree Java, in-tree Rust, and the seven
corpora manifests (whose roots do exist on this host):

| population | consumers that BRANCH on the value | note |
|---|---|---|
| in-tree first-party Java | **0** | see below |
| in-tree Rust | **0** | every occurrence is a producer or a registry-presence assertion |
| h2, spring-framework, hibernate-orm, keycloak, commons-math | **0** | no `getCipherSuite` in any of them |
| **Tomcat** | **4** | the real consumers, below |
| bc-java | **0 reachable** (14 apparent) | see the dispatch note |
| netty | n/a | **not present in this environment** — no manifest, no checkout |

**Tomcat is the population that matters, and it validates two of this lane's
three fixes by name:**

* `JSSESupport.java:171` — `byte[] ssl_session = session.getId(); if
  (ssl_session == null || ssl_session.length == 0) { return null; }`
  **Tomcat tests `length == 0` explicitly.** CratonVM's 32 fabricated bytes are
  precisely the value that defeats this check, so an unhandshaken session
  presents as a trackable one. This is the consumer for fix 3.
* `JSSESupport.java:161` — `return keySizeCache.get(session.getCipherSuite());`
  — a **map key** over a table built from `Cipher.values()`' JSSE names.
  `SSL_NULL_WITH_NULL_NULL` misses, so `keySize` is null and Tomcat
  correctly suppresses the `SSL_CIPHER_USEKEYSIZE` request attribute. The
  fabricated `TLS_AES_256_GCM_SHA384` **hits**, so Tomcat publishes
  `SSL_CIPHER_USEKEYSIZE=256` for a session that negotiated nothing. That is
  the concrete harm, with a file and a line.
* `ResolverImpl.java:163-171` and `:177-183` — `OpenSSLCipherConfigurationParser
  .parse(cipherSuite)` then `cipherList.size() == 1`, for `SSL_CIPHER_EXPORT`
  and `SSL_CIPHER_ALGKEYSIZE`. An unparseable name yields size 0 and the rewrite
  variable is simply absent — the safe direction.

**The bc-java question, resolved without a build.** 14 assertions of the form
`assertFalse("SSL_NULL_WITH_NULL_NULL".equals(session.getCipherSuite()))` exist
in bc-java's JSSE provider tests, and would flip red *if* CratonVM's
`javax/net/ssl/SSLSession` natives intercepted dispatch onto BC's own
`ProvSSLSession`. They do not, for two independent reasons:

1. `vm/src/runtime/interpreter/invoke.rs:3271` — for a virtual call
   (`walk_native_hierarchy == false`) the native lookup returns `None` as soon
   as the receiver's own class declares the method in bytecode, which
   `ProvSSLSession` does. The hierarchy walk below it follows `superclass`
   only and never reaches an interface.
2. **An in-tree witness already proves it empirically.**
   `regression-suite/src/RForeignLayoutJdkInterfaces.java:316` asserts
   `"SENTINEL-sess-getCipherSuite".equals(sess.getCipherSuite())` on a
   `java.lang.reflect.Proxy` implementing `SSLSession`, and that vector is in
   `CORE_CLASSES` — i.e. expected green. If the native intercepted a Java
   implementor, **that fixture would already be red today.**

Reason 2 covers the Proxy shape by measurement; reason 1 covers the plain-class
shape by reading the dispatcher. The plain-class shape is not measured, so the
honest denominator is **0 reachable, argued from the dispatcher and witnessed
for the adjacent shape** — not "0, obviously".

**Net: nothing in-tree branches on these values, and the only external
population that does is Tomcat, where two of the four consumers get strictly
better answers.** The change is safe to land and its riskiest edge is not a
consumer at all — it is `getEnabledProtocols`, §5.

## 5. What landed, in `native-builtins/src/phases_late/ssl_security.rs` (this lane's file)

**One place now knows the two spellings**, per E3-1's lesson that a family
spread over N sites drifts:

```rust
pub(crate) const JSSE_NULL_CIPHER_SUITE: &str = "SSL_NULL_WITH_NULL_NULL";
pub(crate) const JSSE_NULL_PROTOCOL: &str = "NONE";
pub(crate) fn new13_alloc_null_ssl_session(ctx, tls_id) -> Result<ObjectRef, …>
```

carrying the §1/§2 transcripts at the declaration, so a reader who greps the
literal lands on the measurement and not on this document.

| # | site | before | after |
|---|---|---|---|
| 1 | `new13_resolve_socket_session` | `Value::Object(None)` for an unconnected socket | the null session; **deliberately not cached** into `NEW13_SOCK_SESSION`, so a later `connect()` still builds a real one |
| 2 | `new13_alloc_ssl_session` fallback | `("TLSv1.3", "TLS_AES_128_GCM_SHA256")` | `(NONE, SSL_NULL_WITH_NULL_NULL)` |
| 3 | the layered-handshake session builder | `("TLS", "UNKNOWN", None, None)` | the sentinel pair |
| 4 | `SSLSession.getProtocol` fallback | `ctx.get_field(this, PROTO)` raw — a **null String** when the slot is not a String | the sentinel |
| 5 | `SSLSession.getCipherSuite` fallback | same | the sentinel |
| 6 | `SSLSession.getId` | 32 fabricated bytes, **all seeded from `tls_id` alone on a miss** so every never-connected session shared one id | `byte[0]` when no stream is known |
| 7 | `SSLEngine.getSession` (synthetic-mode) | `("TLSv1.3", "TLS_AES_128_GCM_SHA256")` | the sentinel pair |
| 8 | `SSLSocket.getEnabledProtocols` | see below | see below |

### Site 8 is what this change UNMASKED, and it is the reason to read a fix twice

`getEnabledProtocols` reads the negotiated protocol out of the session returned
by `new13_resolve_socket_session`. With site 1 in place and nothing else, an
unconnected socket would have started reporting **`["NONE"]`** — a worse answer
than the one being fixed, and exactly the shape this directory records as *a
fix that only pins the positive half*. `getEnabledProtocols` is about
**configuration**, not negotiation, so the sentinel must be filtered there.
Measured, `scratchpad/e12/E12Enabled.java`:

```
UNCONNECTED SSLSocket:
  getEnabledProtocols()    = [TLSv1.3, TLSv1.2]
  session.getProtocol()    = NONE
  getEnabledProtocols() contains "NONE" = false
```

so the no-negotiation answer is the enabled **list**. The old code's
`unwrap_or("TLSv1.3")` was a one-element guess in the one case HotSpot answers
with two; it now answers both. The fixture pins this arm
(`socket.getEnabledProtocols.hasSentinel`).

### Two unit tests, because a document cannot fail a build

E3-1's own conclusion was that the seven unfixed sites survived a day *because
the family was named in a document instead of in code*. So:

* `the_null_session_sentinels_are_the_measured_jsse_spellings` — pins both
  literals with the transcript in the assertion message, **and** asserts the
  sentinel is absent from `t27_tls::SUPPORTED_CIPHER_SUITE_NAMES`. That third
  assertion is §2's argument made breakable: if the sentinel ever becomes
  offerable, the reason for returning it is gone and the build says so.
* `null_session_accessors_answer_the_sentinels_not_a_null_string` — drives the
  four registered accessors over a 3-field session with `tls_id = -1`.
* `a_populated_session_is_not_overwritten_by_the_sentinel` — the **mutation
  check**. Without it, both accessors could return the sentinel
  unconditionally and the previous test would still pass, measuring one branch
  and calling it coverage.

### Registration reality — which of these edits is actually LIVE

Read the ordering before believing any prediction here.
`lib.rs:18474 register_p68_ssl` → `18497 net_phase_e` → `18535
t27_tls::register_sslengine_real`, and registration is last-write-wins. So on
`javax/net/ssl/SSLSession`, **in the default real-JDK mode (which is what
`--jdk-only` runs) `t27_tls`'s copies of `getProtocol`, `getCipherSuite`,
`getId`, `isValid`, `getCreationTime`, `getLastAccessedTime` and
`getPeerCertificates` WIN, and this file's are dead.** Under `--synthetic-jdk`,
`register_phase68_natives` (lib.rs:24103) re-runs `register_p68_ssl` after
`register_tls_natives` (24100) and this file's win back.

| edit | live in real-JDK / `--jdk-only`? | live in `--synthetic-jdk`? |
|---|---|---|
| 1 `new13_resolve_socket_session` (via `SSLSocket.getSession`, not re-registered anywhere) | **YES** | YES |
| 2, 3, 7 — the PRODUCERS that populate the session slots | **YES** (t27's winning accessors read the slots these write) | YES |
| 8 `getEnabledProtocols` (not re-registered anywhere) | **YES** | YES |
| 4, 5, 6 — the ACCESSOR fallbacks | **NO — overwritten by `t27_tls`** | YES |

**This is why NOMINATIONS 1-4 are not optional.** Edits 4-6 are correct and
inert in the mode this lane exists to serve; the same three answers have to be
made in `t27_tls.rs` by whoever owns it. The producer edits carry the cipher
and protocol fix into real-JDK mode on their own, so **the fabricated cipher is
half-fixed by this commit and half-nominated** — `build_synthetic_ssl_session`
is a producer this lane does not own.

## 6. The fixture — `regression-suite/src/RSslNullSession.java` (new, this lane's to create)

47 checks, **no network**: every arm uses an object that was never connected,
so it cannot bind, resolve, dial, or flake on a port. Three doors:

* **DOOR 1** — `(SSLSocket) SSLSocketFactory.getDefault().createSocket()`. This
  is the exact object this file's own `createSocket()V` registration mints with
  `NEW13_SOCK_TLSID = -1`, so the arm reaches the defect rather than reporting
  its own reach. Then the same socket again after `close()`.
* **DOOR 2** — `SSLContext.getInstance("TLS").init(null,null,null)
  .createSSLEngine()`, before any handshake.
* **The argument, mechanised** — `SSL_NULL_WITH_NULL_NULL` must NOT be in
  `getSupportedCipherSuites()`, and `TLS_AES_256_GCM_SHA384` /
  `TLS_AES_128_GCM_SHA256` MUST be. If either of the latter two ever reads
  false, §2's reasoning has to be re-derived, not the code.

HotSpot 25.0.3+9-LTS, three consecutive runs, byte-identical:

```
CK RSslNullSession socket.session = non-null
CK RSslNullSession socket.getCipherSuite = SSL_NULL_WITH_NULL_NULL
CK RSslNullSession socket.getProtocol = NONE
CK RSslNullSession socket.getId.length = 0
CK RSslNullSession socket.isValid = false
CK RSslNullSession socket.getPeerCertificates = javax.net.ssl.SSLPeerUnverifiedException
CK RSslNullSession socket.getLocalCertificates = null
CK RSslNullSession socket.getEnabledProtocols = [TLSv1.3, TLSv1.2]
CK RSslNullSession engine.getHandshakeSession = null
CK RSslNullSession unofferable.sentinelIsSupported = false
CK RSslNullSession checks=47 failures=0
PASS RSslNullSession (47 checks)
```

**PREDICTED CratonVM, before this lane:** DOOR 1 dies at
`socket.session = null`, then `NullPointerException` — the vector does not even
reach the cipher assertion. DOOR 2 reports
`engine.getCipherSuite = TLS_AES_256_GCM_SHA384`, `engine.getProtocol = TLSv1.3`,
`engine.getId.length = 32`, `engine.isValid = true`,
`engine.getHandshakeSession = <non-null>`.

**PREDICTED after this lane's edits alone:** DOOR 1 passes. DOOR 2 **still
fails** — it goes through `build_synthetic_ssl_session`, which is NOMINATION 1.
That is deliberate and is the point of a discriminating fixture.

## 7. Residuals

1. **Nothing here was built or run against CratonVM.** Rust type-checked
   standalone only.
2. **`isValid()` after socket close.** Measured: HotSpot keeps `isValid() ==
   true` after `close()` (arm G) — the session outlives the transport, for
   resumption. CratonVM ties validity to a live stream and will say `false`.
   Not fixed: correcting it needs a per-session invalidated flag *and* a
   registered `invalidate()` writer (p68 registers none), which is a state
   model, not a fallback. The fixture does not assert it.
3. **`getCreationTime()` is a time series, not a constant.** p68's version
   returns `now` on every call, so two reads of one session disagree; HotSpot's
   is fixed at creation (identical in arms D/E and F/G/H). t27's version, which
   wins in real-JDK mode, returns **0** for any session with fewer than 6
   fields, and HotSpot never returns 0. Both wrong, differently. NOMINATION 5.
4. **`getPeerHost()`/`getPeerPort()` on a handshaked session.**
   `build_synthetic_ssl_session` hardcodes `null` / `-1`, which is right for the
   null session and wrong after a handshake (HotSpot returns the *requested*
   host/port, not the certificate's). Out of this lane's scope; noted because
   it is in the same constructor as NOMINATION 1.
5. **`SSLSocket.getSSLParameters()`'s protocol** (`ssl_security.rs:3506`) has
   the same one-element `unwrap_or("TLSv1.3")` guess as site 8 did. It reads
   the RAW `NEW13_SOCK_SESSION` field, which this lane never populates with a
   null session, so **it cannot leak the sentinel** — but it still guesses.
   Left alone to keep this commit reviewable.
6. **`getApplicationBufferSize` = 16384** — see §1 and NOMINATION 6.

---

## NOMINATION 1 — `native-builtins/src/t27_tls.rs`, `build_synthetic_ssl_session` (**the fabrication**)

**This is the site E3-1 named and the one this record exists for.** Not this
lane's file.

REPLACE (currently at `:9553`-`:9568`):

```rust
        let cipher = s
            .conn
            .as_ref()
            .and_then(|c| c.negotiated_cipher_suite())
            .map(negotiated_suite_name)
            .unwrap_or_else(|| "TLS_AES_256_GCM_SHA384".into());
        let alpn = s.negotiated_alpn.clone().unwrap_or_default();
        (proto.to_string(), cipher, alpn)
    })
    .unwrap_or_else(|| {
        (
            "TLSv1.3".into(),
            "TLS_AES_256_GCM_SHA384".into(),
            String::new(),
        )
    });
```

WITH:

```rust
        // E12: JSSE's own answer for "no cipher negotiated", NOT a guess. The
        // literal that used to stand here is in HotSpot's SUPPORTED suite
        // list, so a caller could not tell this fallback from a real TLS 1.3
        // handshake — and security-sensitive code branches on this string.
        // The sentinel is UNOFFERABLE (`setEnabledCipherSuites` throws
        // IllegalArgumentException for it), so it can never be confused with
        // a negotiation. Measured: docs/known-issues/jdk-only/
        // E12-1-the-null-session-and-the-fabricated-cipher.md §1-§2.
        let cipher = s
            .conn
            .as_ref()
            .and_then(|c| c.negotiated_cipher_suite())
            .map(negotiated_suite_name)
            .unwrap_or_else(|| {
                crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.into()
            });
        let alpn = s.negotiated_alpn.clone().unwrap_or_default();
        (proto.to_string(), cipher, alpn)
    })
    .unwrap_or_else(|| {
        (
            crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL.into(),
            crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.into(),
            String::new(),
        )
    });
```

**Also, in the same function, the protocol arm at `:9548`-`:9552`:**

REPLACE:

```rust
        let proto = match s.conn.as_ref().and_then(|c| c.protocol_version()) {
            Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
            Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
            _ => "TLSv1.3",
        };
```

WITH:

```rust
        let proto = match s.conn.as_ref().and_then(|c| c.protocol_version()) {
            Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
            Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
            // E12: no connection means no negotiated version. `"TLSv1.3"`
            // here reported the VM's DEFAULT as though it were the outcome.
            _ => crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL,
        };
```

**Visibility:** `JSSE_NULL_CIPHER_SUITE` / `JSSE_NULL_PROTOCOL` are already
`pub(crate)` and this lane owns their file, so no cross-file change is needed
to consume them. If the owner of `t27_tls.rs` would rather not take the
dependency, inline the two literals — but then please add the constants' §2
comment at the site, because the whole failure mode is a spelling that drifts.

## NOMINATION 2 — `t27_tls.rs`, the same function's `isValid` slot (`:9577`)

REPLACE:

```rust
    ctx.set_field(ses, 2, Value::Int(1));
```

WITH:

```rust
    // E12: slot 2 is the `isValid` flag, and this constructor also serves
    // engines that have NEVER handshaked. HotSpot answers `isValid() == false`
    // for a session with no negotiation (measured, E12-1 §1) — writing 1
    // unconditionally is a third fabrication beside the cipher and the id.
    // A session is valid iff a connection actually negotiated something.
    let negotiated = with_engine(id, |s| {
        s.conn
            .as_ref()
            .and_then(|c| c.negotiated_cipher_suite())
            .is_some()
    })
    .unwrap_or(false);
    ctx.set_field(ses, 2, Value::Int(if negotiated { 1 } else { 0 }));
```

**Sequencing caution:** this must land WITH NOMINATION 1, not before it. On its
own it makes `isValid()` false while `getCipherSuite()` still answers
`TLS_AES_256_GCM_SHA384` — a session that reports a strong suite and denies
being valid, which is more confusing than either bug alone.

## NOMINATION 3 — `t27_tls.rs`, `getId` (`:11624`) — the second fabrication

REPLACE:

```rust
    r.register(cls, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let seed = gc_stable_objref_key(ctx, this);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
```

WITH:

```rust
    r.register(cls, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // E12: a session that negotiated nothing has NO id, and JSSE says so
        // with byte[0] rather than 32 plausible bytes. Tomcat's
        // `JSSESupport.getSessionId` (java/org/apache/tomcat/util/net/jsse/
        // JSSESupport.java:171) tests exactly `ssl_session.length == 0`, so
        // the fabricated 32 bytes make an unhandshaken session present as a
        // trackable one. Slot 2 is the `isValid` flag on the 7+-field shape;
        // shorter shapes carry a stream id and are only minted post-handshake.
        let negotiated = ctx.object_num_fields(this) < 7
            || matches!(ctx.get_field(this, 2), Value::Int(v) if v != 0);
        if !negotiated {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
        let seed = gc_stable_objref_key(ctx, this);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
```

**Depends on NOMINATION 2** — slot 2 is only a truthful "did we negotiate"
signal once NOMINATION 2 stops writing `1` unconditionally. Landing this alone
is a no-op, not a regression.

## NOMINATION 4 — `t27_tls.rs`, `getProtocol` / `getCipherSuite` (`:11644`, `:11653`)

Both return `ctx.get_field(this, slot)` raw, so a slot the layout guard dropped
yields a **null String** from a method JSSE contracts to be non-null.

REPLACE:

```rust
        Ok(Some(ctx.get_field(this, slot)))
    });
```

WITH (in `getProtocol`):

```rust
        // E12: never a null String — see E12-1 §2.
        match ctx.get_field(this, slot) {
            v @ Value::Object(Some(_)) => Ok(Some(v)),
            _ => {
                let s = ctx.create_string(
                    crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL,
                );
                Ok(Some(Value::Object(Some(s))))
            }
        }
    });
```

and the identical shape in `getCipherSuite`, with `JSSE_NULL_CIPHER_SUITE`.
(The two bodies are textually distinct — `getProtocol` ends `}));\n    });`
and `getCipherSuite` ends inside an `r.register(` block — so apply them
separately rather than with a global replace.)

## NOMINATION 5 — `t27_tls.rs`, `getHandshakeSession` (`:10055`) and `getCreationTime` (`:11685`)

Two smaller shapes, grouped because both are "a plausible value where HotSpot
has a specific one":

* **`getHandshakeSession`** returns a populated synthetic session
  unconditionally. Measured (§1): HotSpot returns **null** before
  `beginHandshake()` and **null** after the handshake finishes, non-null only
  in between. The site's own comment says "or null outside a handshake" and
  then does not do that. Gate it on `with_engine(id, |s| s.conn.is_some() &&
  !s.handshake_complete)` or the local equivalent, and return
  `Value::Object(None)` otherwise. **Check Jetty's `SslConnection.getBufferSize()`
  first** — the comment says that caller is why the non-null exists, and it
  calls `getHandshakeSession()` *before* any handshake, i.e. exactly where the
  correct answer is null. This one needs a run, not just an edit.
* **`getCreationTime`/`getLastAccessedTime`** return `Value::Long(0)` for any
  session with ≤5 fields. HotSpot returns a real epoch value in **every** state
  including the null session (§1), and 0 is a value application code reads as
  "no session". Answer `epoch_millis()` captured at construction for the short
  shapes, or widen them.

## NOMINATION 6 — the buffer-size comment is measurably wrong, in two files

`t27_tls.rs:11595`-`:11617` and the twin in `tls.rs` (~`:1108`-`:1136`) both
carry a **"KEEP (correct constants…)"** header asserting the real JDK returns
16384 for `getApplicationBufferSize`. Measured on HotSpot 25.0.3+9-LTS:
**16704** on the null session and **16676** after a TLS 1.3 handshake. 16709
for `getPacketBufferSize` is correct.

**No code edit is proposed**: 16384 is a safe under-report against this VM's own
engine, and raising it without checking every `BUFFER_OVERFLOW` path is how a
constant becomes an outage. What must change is the comment, in both copies,
because a confident "KEEP: correct" is what stops the next reader from
measuring. Replace the parenthetical with the two measured numbers and a note
that the value varies with the negotiated suite's expansion, which is why it is
not a constant in real JSSE.

## NOMINATION 7 — `t27_tls.rs`, the six producer fallbacks (one shape, six sites)

Every rustls stream constructor spells the unnegotiated case by hand:

| line | expression |
|---|---|
| `:3279`, `:3443`, `:3627`, `:3702` | `_ => "TLS",` (the protocol match arm) |
| `:3286`, `:3450`, `:3633`, `:3708` | `.unwrap_or_else(\|\| "UNKNOWN".to_string())` |
| `:3465`, `:3478` | `"UNKNOWN".to_string(),` (the two legacy-acceptor tuples) |
| `:4947` | `.unwrap_or_else(\|\| ("TLSv1.3".into(), "UNKNOWN".into(), None, None))` |
| `:5930` | `.unwrap_or_else(\|\| "?".into())` — `run_loopback_self_test`, VM-private |

All should become `JSSE_NULL_PROTOCOL` / `JSSE_NULL_CIPHER_SUITE`. These are
lower-priority than 1-4: each sits immediately after a handshake that
succeeded, so reaching them is an internal inconsistency rather than an
ordinary state — but they are the values that then flow through
`rustls_session_info` into `SSLSession`, and `"UNKNOWN"` is not a JSSE word.
`:5930` is cosmetic (diagnostic string only). This is the same "eight producers,
one helper" shape E3-1 counted; **the count is now six raw spellings in one
file, and it should be one.**

## NOMINATION 8 — `regression-suite/run.sh` (not this lane's file)

Register the new vector. In `CORE_CLASSES` (`run.sh:151`), append to the
existing single-line list:

REPLACE the trailing fragment:

```
RJdkOptionalShape RSimpleDateFormatZone"
```

WITH:

```
RJdkOptionalShape RSimpleDateFormatZone RSslNullSession"
```

**Read this before landing it.** `RSslNullSession` is **PREDICTED RED on
CratonVM** until NOMINATION 1 lands, because DOOR 2 goes through
`build_synthetic_ssl_session`. Three options, in the order I'd take them:

1. Land NOMINATION 1 first, then this. Best outcome.
2. Land this now and accept a red vector that names a real, measured defect.
   Defensible — the suite's own coverage census exists to stop vectors being
   silently unscheduled — but it moves the green baseline of a plain
   `bash regression-suite/run.sh`, which the file's own comments treat as
   load-bearing.
3. Park it in `UNREGISTERED_CLASSES` (`run.sh:202`) with the reason
   *"discriminates a defect whose other half is NOMINATION 1 of E12-1; schedule
   when that lands"*. The file requires a reason for every entry, and this one
   has a specific unblocking condition rather than "someone's head".

**Do not** put it in `JDKONLY_CLASSES`: the defect is present in the DEFAULT
real-JDK mode (§5's live/dead table), so scheduling it only under `--jdk-only`
would under-report it.

## How to verify

Cheapest first. Every CratonVM row is PREDICTED.

1. **`cargo test -p cratonvm-native-builtins new13_tests`** — the three unit
   tests of §5. No VM, no network. The mutation check is the one that matters:
   comment out the sentinel arm and
   `a_populated_session_is_not_overwritten_by_the_sentinel` must still pass
   while the other fails.
2. **`bash regression-suite/run.sh` with `ONLY="RSslNullSession"`** once
   NOMINATION 8 lands. HotSpot's 47 lines are in §6 and are byte-stable.
3. **`--dump-native-registry`**, flags BEFORE `-cp` or they are silently
   ignored. Confirm which registrar owns `javax/net/ssl/SSLSession.getProtocol`
   / `getCipherSuite` / `getId`. §5's live/dead table is derived from source
   order plus `lib.rs`'s own ordering comments; **the dump is the authority and
   this record's predictions for edits 4-6 depend on it.**
4. **The embedded-Tomcat HTTPS fixture** — after a real handshake nothing here
   should change at all. Every edit is on a path that only runs when nothing
   was negotiated, so a behaviour change on a successful connection means one
   of the fallbacks was being reached in the success path, which would be a
   finding in its own right.

| | before | HotSpot 25 (MEAS) | after (PRED) |
|---|---|---|---|
| unconnected `SSLSocket.getSession()` | **null** | `SSLSessionImpl` | non-null null-session |
| … `.getCipherSuite()` | **NullPointerException** | `SSL_NULL_WITH_NULL_NULL` | `SSL_NULL_WITH_NULL_NULL` |
| … `.getProtocol()` | NPE | `NONE` | `NONE` |
| … `.getId().length` | NPE | `0` | `0` |
| … `.getEnabledProtocols()` | `[TLSv1.3]` | `[TLSv1.3, TLSv1.2]` | `[TLSv1.3, TLSv1.2]` |
| pre-handshake `SSLEngine.getSession().getCipherSuite()` | `TLS_AES_256_GCM_SHA384` | `SSL_NULL_WITH_NULL_NULL` | unchanged until **NOM 1** |
| pre-handshake `SSLEngine.getHandshakeSession()` | non-null | `null` | unchanged until **NOM 5** |
| any handshaked session | the negotiated values | the negotiated values | **unchanged** |
