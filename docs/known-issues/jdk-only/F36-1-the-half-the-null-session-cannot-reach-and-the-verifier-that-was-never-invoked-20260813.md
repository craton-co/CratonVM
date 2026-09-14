# F36-1 — the half `RSslNullSession` structurally cannot reach, and the HostnameVerifier that was never invoked

**Status: FIXTURES ONLY. Nothing under `native-*/src/` was touched.** One new
regression vector, one extended one, and this record. Every expected value below
was **MEASURED on Microsoft OpenJDK 25.0.3+9-LTS** (`Microsoft-13877124`) on this
host before it was written.

**Prov: HotSpot column MEAS (this host, `scratchpad/f36/`). CratonVM column NOT
MEASURED AT ALL** — this lane was forbidden to build or run CratonVM, and no row
below carries a CratonVM number, predicted or otherwise. §7 names the rows most
likely to be the first red and why.

| file | why it is mine |
|---|---|
| `regression-suite/src/RSslLiveSession.java` | **new**, assigned (F25-1 NOMINATION 1) |
| `regression-suite/src/RJdkSecurity.java` | assigned (F25-1 NOMINATIONS 2, 3, 4) |
| this document | assigned |

`regression-suite/run.sh` and `regression-suite/src/RSslNullSession.java` are
**not** mine. Both are handed back as nominations with exact literal text, and
**NOMINATION 1 is required for the new vector to run at all** — until it lands,
`RSslLiveSession.java` compiles, never executes, and produces a COVERAGE WARNING
on every run that would be FATAL under `STRICT_COVERAGE=1`.

---

## 0. The denominators, in one table

Say these out loud in any handoff. A silent denominator change reads as a pass.

| class | family | before | after | delta |
|---|---|---|---|---|
| `RJdkSecurity` | `srArgKinds` (its own tripwire) | **43** | **52** | +9 |
| `RJdkSecurity` | `tls()` (no tripwire — class total only) | — | — | +17 |
| `RJdkSecurity` | **class total** | **123** | **149** | +26 |
| `RSslLiveSession` | `handshake` | — | **25** | new |
| `RSslLiveSession` | `attrs` | — | **17** | new |
| `RSslLiveSession` | `distinct` | — | **6** | new |
| `RSslLiveSession` | `invalidate` | — | **14** | new |
| `RSslLiveSession` | `verifier` | — | **11** | new |
| `RSslLiveSession` | `serverSide` | — | **15** | new |
| `RSslLiveSession` | `drainTrap` | — | **7** | new |
| `RSslLiveSession` | **class total** | — | **95** | new |

`RJdkSecurity`'s `srArgKinds` tripwire literal moved with it — the family throws
`AssertionError: srArgKinds ran N checks, header says 52` otherwise, and that
tripwire was re-checked by changing the literal and confirming it fires.

Produced by running the fixtures, never by adding on paper:

```text
CK RJdkSecurity srArgKinds=52
CK RJdkSecurity checks=149
PASS RJdkSecurity (149 checks)

CK RSslLiveSession handshake=25
CK RSslLiveSession attrs=17
CK RSslLiveSession distinct=6
CK RSslLiveSession invalidate=14
CK RSslLiveSession verifier=11
CK RSslLiveSession serverSide=15
CK RSslLiveSession drainTrap=7
CK RSslLiveSession fails=0
CK RSslLiveSession checks=95
PASS RSslLiveSession (95 checks)
```

---

## 1. The correction: F18's "the verifier's session is a different object" is an artefact of a verifier that was never called

`F18-1` §8.3(4) records, as a measured fact, that

> the object handed to a `HostnameVerifier` is **a different object** from the
> one `getSSLSession()` returns

and its transcript carries the row `getSSLSession == verifier session = false`.

**That row was comparing against `null`.** `HttpsURLConnection` consults a custom
`HostnameVerifier` **only after its own endpoint identification has already
failed** (`HttpsClient.afterConnect` → `checkURLSpoofing`, which runs
`HostnameChecker` first and calls the verifier only in the `!ok` branch). F18's
probe set a verifier, connected to `https://localhost`, and used a `keytool`
certificate that matches `localhost` — so the built-in check succeeded, the
verifier was never invoked, its capture slot stayed `null`, and `null == session`
is `false` for the obvious reason.

Measured here, with the invocation asserted **first** so the comparison cannot be
made against a null again:

```text
=== verifier reached, https://127.0.0.1, SAN has dNSName=localhost and no iPAddress ===
  verifier invoked                    = true
  verifier host argument              = 127.0.0.1
  verifier session == getSSLSession() = true        <-- SAME object
  verifier session .isValid()         = true
  verifier session .getId().length    = 32
  verifier session .getPeerPrincipal  = CN=localhost
  verifier session .getSessionContext = sun.security.ssl.SSLSessionContextImpl

=== the control, https://localhost, same verifier, same certificate ===
  verifier invoked                    = false       <-- F18's configuration
```

Both halves are in one run, which is what makes this a correction rather than a
counter-claim: the second block **reproduces F18's false negative**, and the
first shows what the same question answers once the door is actually opened.

**Why this matters beyond tidiness.** F10-1 §2 repaired *two* minters, and
`huc_verify_hostname` is the second one — the site where the old `-1` was
self-contradicting, because a verifier is invoked to decide whether to **accept**
the peer and was being told the handshake it had been invoked to vet had not
happened. A fixture that captures a null there asserts nothing about that minter.
The `verifier` family is 11 rows, and **row 2 (`verifier.invoked`) and row 11
(the control) are what make the other nine mean anything.**

This is the same species of defect as `[neg≠ruled out]` in this workspace's
memory: *witness the probe SETUP, or the negative is worthless.*

---

## 2. `RSslLiveSession` — what `RSslNullSession` structurally cannot assert

F25-1 §2.5 states the constraint exactly: `RSslNullSession`'s **no-network
property is load-bearing**, so two measured halves of F18's work had no row
anywhere in the tree. Both now have one.

### 2.1 `invalidate()` moves TWO accessors — family `invalidate`, 14 rows

F18-1 §3.1 corrected an "isValid and nothing else" claim repeated in **four
comments across three files**. The second accessor is `getSessionContext()`, and
it is only visible where a live context exists to be dropped.

```text
before invalidate():  getSessionContext() = sun.security.ssl.SSLSessionContextImpl
after  invalidate():  isValid()           = false
                      getSessionContext() = null          <-- THE ROW
                      getId()             byte-for-byte identical, length 32
                      getCipherSuite()    unchanged
                      getProtocol()       unchanged
                      getPeerPrincipal()  unchanged
                      getPeerCertificates() 1, unchanged
  second invalidate() returns normally; isValid() still false
```

Two rows exist only to stop a wrong implementation passing the twelve above:
`inv.other.stillValid` and `inv.other.sessionContext.isNull`, asked of the
**other** live session. An implementation that invalidated the whole
`SSLSessionContext` rather than the session satisfies every row before them.

And `inv.before.sessionContext.isNull` is asserted **first**, because without it
the headline row is vacuous: dropping to `null` from `null` is not a drop.

### 2.2 `getPeerPrincipal()` and the leaf subject cannot legally disagree — family `handshake`

```text
getPeerPrincipal()             = CN=localhost
getPeerPrincipal().getClass()  = javax.security.auth.x500.X500Principal
peerCerts[0].getSubjectX500Principal().equals(getPeerPrincipal())  = true
getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal())  = true
```

**Both directions of `equals` are separate rows on purpose.** These are two
distinct objects that must compare equal, and on CratonVM they come from two
different tables — F10-1 NOMINATION 2: `t27_tls`'s `getPeerCertificates` reads
the session-keyed `session_peer_certs_table`, while `ssl_security`'s
`getPeerPrincipal` reads `s2_tls_peer_cert_chain_der(slot2)`, the socket
registry, which has no entry for a connection that was never a registered socket.
An implementation can get one direction and not the other. That is F18's point
restated as an assertion: it is what makes **one shared resolver** correct rather
than merely tidy.

### 2.3 The sharpest vector in the file — family `serverSide`, 15 rows

One genuinely negotiated session, asked two questions:

```text
getLocalPrincipal()   = CN=localhost   (a javax.security.auth.x500.X500Principal)
getLocalCertificates()= 1 certificate
getPeerPrincipal()    THROWS javax.net.ssl.SSLPeerUnverifiedException: peer not authenticated
getPeerCertificates() THROWS javax.net.ssl.SSLPeerUnverifiedException: peer not authenticated
isValid()             = true
getId().length        = 32
getSessionContext()   = sun.security.ssl.SSLSessionContextImpl
```

**"Negotiated" and "peer authenticated" are different questions**, and an
implementation that answers the second from the first is wrong in whichever
direction it guesses. `RSslNullSession` gets the same refusal *message* from a
session that negotiated nothing, so on that vector the two questions have the
same answer and the distinction is invisible. Here they diverge inside one
object, and the message is asserted as well as the class — F25-1 §2.2(c)'s point,
which this directory keeps re-learning from the other end (an `IOException` whose
message names the right thing still never matches the caller's `catch`).

This is also the only family whose rows are **captured on the accept thread and
replayed through `ck` on the main thread**. Asserting in place would put a
failure on a thread nobody joins.

### 2.4 `getSSLSession()` must return the same object twice — F10-1 NOMINATION 1, first executable form

`client.sslSession.sameObjectTwice` is `true` on HotSpot. F10-1 §6.1 records that
CratonVM's six accessors each call `https_session_object`, which **allocates**,
and that this was invisible while every call returned `byte[0]`. Once the ids are
real it is the most visible remaining divergence on that path, because `getId()`
is seeded from the object's identity — two calls would then disagree about one
connection. This row is the executable form of a nomination that has never run.

### 2.5 The attribute map on a WIDE session — family `attrs`, 17 rows

E31-1 §2's recorded defect is that slot 3 is the peer host on the 6- and 8-field
session shapes and the **attribute map** on the 4-field one, so a width-blind
read returns a `java.util.HashMap` through a `()Ljava/lang/String;` descriptor as
soon as anything has called `putValue` — and Jetty's
`SecureRequestCustomizer.retrieveSni()` does, on every SSL request.

`RSslNullSession`'s `attributes` arm arms that trap on the **narrow** shape. This
family arms it on the shape a real handshake produces, where `getPeerHost()` has
a real answer to lose. **These are different rows in the width table and
therefore different bugs**; the four rows before them (`putValue` returns,
`getValue` round-trips, its class is `java.lang.String`, `getValueNames()` names
it) exist so the three shadow rows cannot pass for a VM whose `putValue` silently
did nothing.

The null contract is F25-1 NOMINATION 4, asserted here on the **live** session and
in `RJdkSecurity` on the **null** one — see §5.3.

---

## 3. Two construction decisions, and why each is not the obvious one

### 3.1 The key material is built in this process, and the claim that it could not be is false

`RJdkSecurity`'s own header said, and `jdk-only-coverage.txt` §3 still says, that
a real loopback handshake

> needs a key store, and generating a self-signed certificate portably requires
> internal `sun.security` APIs.

**Measured false.** The certificate is assembled as DER by hand — v3, fixed
serial, `sha256WithRSAEncryption`, a one-RDN `CN=localhost` issuer and subject, a
validity window from 2001 to 2049, `PublicKey.getEncoded()` used verbatim as the
`SubjectPublicKeyInfo` (which is exactly what X.509 encoding already is), and one
`subjectAltName` extension — signed with
`Signature.getInstance("SHA256withRSA")` and read back through
`CertificateFactory`. Every one of those is public API. It goes into a
`KeyStore.getInstance("PKCS12")` opened with `load(null, null)`, which never
touches the disk.

The consequences are the reason it was done this way rather than shipping a
`keytool`-generated `ks.p12`:

* **no binary resource** — `regression-suite/resources/` is not this lane's file,
  and a committed keystore would need its own nomination;
* **no expiry** — a checked-in certificate is a fixture that goes red on a date
  nobody wrote down;
* **nothing to keep in step** — the trust store is the same object graph as the
  key store, so the two cannot drift.

`RJdkSecurity`'s header has been corrected (it is this lane's file).
`jdk-only-coverage.txt` has not (it is not) — NOMINATION 3.

### 3.2 The server is an `SSLServerSocket`, not `com.sun.net.httpserver.HttpsServer`

Both oracle harnesses this vector derives from (`scratchpad/f10/F10HttpsSession.java`,
`scratchpad/f18/F18SessionContract.java`) use `HttpsServer`, and the first draft
here did too — it measured identically. It was replaced, for a reason that only
shows up when you look at the suite rather than the probe:

**no fixture in `regression-suite/src/` depends on `jdk.httpserver`.** Making
this the only one adds a module dependency to the JDK-only corpus for no
question that `javax.net.ssl` cannot ask — the server side of the handshake is
reached through `SSLServerSocket.accept()` and `SSLSocket.getSession()`, which is
also the surface `ssl_security.rs`'s acceptor path serves. Four hand-written
HTTP/1.1 responses replace the container.

Loopback socket binding itself is **not** new: `RJdkNet`, `RChannelInterrupt`,
`RSocketChannelInterrupt` and `RJdkAsyncChannel` are all already scheduled and
all bind `127.0.0.1:0`.

---

## 4. Is a loopback TLS vector reliable enough to schedule? — yes, and here is the evidence

The brief asked for an explicit judgement, and allowed a reasoned "no". The
answer is **yes**, on four grounds, with the one residual risk named.

1. **Nothing is per-run.** The suite diffs both VMs' `CK` lines byte for byte, so
   the vector prints no port, no session id, and no host name derived from a peer
   address. Three consecutive runs are byte-identical — see §6.3.
2. **Nothing is external.** One `SSLServerSocket` on `127.0.0.1:0`, four
   connections to it, no resolution beyond `localhost`/`127.0.0.1`, no file, no
   `keytool`, no clock dependency inside the certificate's 48-year window.
3. **Failure is loud, and never a hang.** This was measured the hard way: the
   first draft threw an assertion and the JVM **hung**, because `HttpsServer`'s
   accept threads are not daemons — the harness would have reported a `TIMEOUT`,
   which is a far worse diagnosis than an assertion. The shipped vector therefore
   has three independent stops: a 20 s timeout on every socket and on connect and
   read; a daemon accept thread; and a `catch (Throwable)` in `main` that reports
   on the `CK` prefix and calls `System.exit(1)`. Behind all of that sits a daemon
   watchdog that prints `CK RSslLiveSession FAILED watchdog-90s phase=<phase>`
   and `Runtime.halt(4)` — so even a wedged native TLS stack names the family it
   died in rather than producing 120 s of silence.
4. **The instrument was checked against its own failure mode.** `[chk instr]`.
   Every row was mutation-checked (§6), so a green run is a run in which 95
   comparisons could each have been lost.

5. **It costs 3-4 s, and getting there found a defect worth more than the
   vector.** The first working version ran in **62 s** against the harness's
   120 s `TIMEOUT` — a 2x margin, which is not a margin. §4.1 is what it was and
   why it matters beyond this file.

**The residual risk, stated plainly: cost, not flakiness.** One RSA-2048 key
generation (520 ms measured) and four TLS 1.3 handshakes, 3-4 s total on HotSpot
on this host. On CratonVM's interpreter it will be slower by whatever that
ratio is. If it ever approaches the cap, the fix is a smaller modulus or a cached
key pair, **not** a longer timeout.

### 4.1 The 62 seconds: `SSLSocket.close()` waits for the peer's `close_notify`

Worth its own section because it is a property of `javax.net.ssl` that any lane
writing a TLS fixture will meet, and because the symptom points away from the
cause.

`SSLSocket.close()` performs the TLS **closure handshake**: it sends
`close_notify` and then waits for the peer's, bounded by `SO_TIMEOUT`. This
vector deliberately leaves response bodies unread until the `drainTrap` family,
so the client sends no `close_notify` until the very end — and the accept thread,
**the only thing that can accept the next connection**, sat in `close()` for the
full 20 s each time. Four connections, ~62 s per run, every run.

Three things about it are worth recording:

* **It is invisible in the output.** The three 62 s runs and the three 3-4 s runs
  are **byte-identical** (`md5 9d86efa94509b107fa91a73f0b89464f` on both sets).
  Nothing in the transcript says "this took a minute"; a vector like this fails
  by TIMING OUT one day on a slower box, not by disagreeing.
* **The obvious diagnosis was wrong twice.** It was first read as the box being
  busy — until `RJdkSecurity` measured 3 s in the same shell. Then as slow DNS,
  because an earlier `HttpsServer` draft reported `getPeerHost()` as
  `kubernetes.docker.internal` on this host — until reverse lookup of `127.0.0.1`
  measured **4 ms**. `[chk instr]`: the instrument that finally named it was a
  seven-line timing probe over resolve/reverse/`SecureRandom`/keygen/`SSLContext`,
  all of which came back fast, which is what left the socket close as the only
  candidate.
* **The fix is one line and it is NOT a longer timeout.** After the response is
  flushed, the accepted socket's `SO_TIMEOUT` drops to 200 ms, so `close()`
  cannot block on a peer that has nothing left to say. The response is already
  written, so nothing the client needs depends on a graceful close. A
  `SocketTimeoutException` from `close()` is then EXPECTED and is explicitly not
  recorded as a server error — otherwise the fix would have turned a 60 s stall
  into a red `server.error` row.

The same shape produced the OTHER robustness fix in the accept loop: a plain
`for (i = 0; i < n; i++)` counted a `SocketTimeoutException` from `accept()` as a
served connection, so on a loaded box a slow client loses its acceptor and the
next `connect()` stalls until the watchdog. Both are in `serve()` and both carry
their measurement in a comment.

One thing that is *not* a reliability question but must not be mistaken for one:
if CratonVM cannot serve TLS on the accept side at all, this vector is red for a
capability reason rather than a contract reason. That is a finding, and §7 says
how to tell the two apart in one line.

---

## 5. `RJdkSecurity` — F25-1 NOMINATIONS 2, 3 and 4

All three were **measured** by F25 and deliberately left unasserted, each with the
same stated precondition: *CratonVM's answer has never been measured, so the row
would be an expectation nobody has checked against the VM.* This lane was assigned
them, so the precondition is **waived by assignment, not by argument**. Every one
of these rows carries a comment saying so, and the rule is: **a red row here is a
finding for `native-builtins/src/securerandom.rs` (or the `SSLSession` attribute
registrar), not a reason to drop the row.**

### 5.1 The algorithm spelling — 3 rows, `srArgKinds` 43 → 46

```text
SecureRandom.getInstance("sha1prng").getAlgorithm()                       = "sha1prng"
SecureRandom.getInstance("sha1prng", "SUN").getAlgorithm()                = "sha1prng"
SecureRandom.getInstance("sha1prng", Security.getProvider("SUN")).getAlgorithm() = "sha1prng"
```

The name is carried through **verbatim**; JCA lookup is case-insensitive and
`getAlgorithm()` is not a normalisation. F12 §3.5 records that
`secure_random_static_provider` normalises to upper-case alphanumeric for the
*lookup* and says nothing about what `getAlgorithm()` then reports.

F25's NOMINATION 2 asks for the first row only. The other two are this lane's, on
this family's own founding lesson: **the two-argument overloads are separate
registrations**, and this tree has a recorded defect
(`Cipher`/`SecretKeyFactory`) in which precisely the `Provider`-object form
behaved differently from its `String`-named twin. A fix that threads the
asked-for spelling through the one-argument body only is invisible without them.

**The row is half a discriminator on its own, and its other half already
existed.** `getInstanceArgumentOrder()`'s `"SHA1PRNG".equals(viaProvider.getAlgorithm())`
asks the same question of an upper-case request. Together they say the answer
**tracks** the request; either alone also passes an implementation that
hard-cases every answer in one direction. Mutants M1–M3 kill the upper-casing
body; that pre-existing row kills the lower-casing one.

Also measured, not asserted, so nobody re-measures: `new SecureRandom()
.getAlgorithm()` is `"DRBG"` on this JDK, and `MessageDigest.getInstance("sha-256")
.getAlgorithm()` is `"sha-256"` — the same verbatim-carry rule in a different
engine.

### 5.2 The three provider messages — 6 rows, `srArgKinds` 46 → 52

F25's NOMINATION 3, all six call sites it enumerates:

```text
getInstance("SHA1PRNG", (String) null)    IllegalArgumentException: missing provider
getInstance("SHA1PRNG", "")               IllegalArgumentException: missing provider
getInstance("SHA1PRNG", (Provider) null)  IllegalArgumentException: missing provider
getInstance("SHA1PRNG", "NOPE")           NoSuchProviderException:  no such provider: NOPE
getInstance("NOPE", "SUN")                NoSuchAlgorithmException: no such algorithm: NOPE for provider SUN
getInstance("SHA1PRNG", "SunJCE")         NoSuchAlgorithmException: no such algorithm: SHA1PRNG for provider SunJCE
```

Three distinct strings across six sites, and **the split is the assertion**:
`"missing provider"` is raised by the ARGUMENT check before any lookup happens,
and the other two are raised BY the lookup. A body that produced one apology for
every provider complaint passes any single row here — which is exactly what
mutant M7 is.

### 5.3 The `putValue`/`getValue` null contract — 17 rows in `tls()`, class total +17

F25-1 NOMINATION 4. Asked of the `SSLEngine`'s pre-handshake session, which is
the session this vector already holds:

```text
putValue(null, "v")   IllegalArgumentException: arguments can not be null    (PLURAL)
putValue("k", null)   IllegalArgumentException: arguments can not be null
putValue(null, null)  IllegalArgumentException: arguments can not be null
getValue(null)        IllegalArgumentException: argument can not be null     (SINGULAR)
removeValue(null)     IllegalArgumentException: argument can not be null     (SINGULAR)
```

**Two methods, two strings, one letter apart.** A single shared constant is wrong
in one of the two places and only a message row can see it. Mutants M11 and M16
are that constant, in both directions, and both die.

`removeValue` is a **third** site with the singular string, which F25's
nomination does not mention — it enumerates `putValue` and `getValue`. Measured
and asserted here.

Eight controls follow, without which every row above also passes a map whose
`putValue` silently does nothing: the round trip, the value's class
(`java.lang.String`, not the map that holds it — mutant M21 is E31-1 §2's
`HashMap` coming back through the value door), `getValueNames()` before and
after, an **absent** name answering `null` rather than refusing, and
`removeValue` of an absent name being a no-op.

**This is deliberately not a duplicate of `RSslLiveSession`'s `attrs` family.**
Same contract, two receivers — and E31-1's recorded defect is a slot whose
meaning depends on the session's WIDTH, so the null-session door and the live
door can and do diverge. If they ever agree on both vectors, that agreement is
itself the finding.

### 5.4 The tautology check on this lane's own rows

F25 found an existing row that was a tautology — `getInstance("SHA1PRNG",
sunProvider).getProvider() == "SUN"` passes even for a body that *discards* the
provider argument, because SUN is also the default for SHA1PRNG.

Every row added by this lane was checked for that shape by writing its mutant
first. The one that needed the most care is §5.1's spelling row, and the answer
is in §5.1: it is genuinely half a discriminator, its other half is named, and
the pair is stated in the source comment so the next reader does not delete one
of them.

---

## 6. Mutation results

### 6.1 `RJdkSecurity` — 26 of 26 died

One mutant per row added, each replacing the expected value with what a **named**
wrong implementation answers, each compiled and run.

| # | row | mutated to (the implementation it names) | |
|---|---|---|---|
| M1 | `getInstance("sha1prng").getAlgorithm()` | `"SHA1PRNG"` — the normalised name the LOOKUP produces | DIED |
| M2 | same, `String`-provider overload | `"SHA1PRNG"` — the 2-arg form normalising where the 1-arg one does not | DIED |
| M3 | same, `Provider` overload | `"SHA1PRNG"` | DIED |
| M4 | `(String) null` message | `"no such provider: null"` — the null routed INTO the lookup | DIED |
| M5 | `""` message | `"missing provider name"` — a special-cased empty string | DIED |
| M6 | `(Provider) null` message | `"null provider"` — the overload's own apology | DIED |
| M7 | `"NOPE"` message | `"missing provider"` — **ONE canned apology for every provider complaint** | DIED |
| M8 | `("NOPE","SUN")` message | `"NOPE SecureRandom not available"` — the ONE-argument template | DIED |
| M9 | SunJCE message | `"no such algorithm: SHA1PRNG"` — a refusal that drops the provider | DIED |
| M10 | `putValue(null,"v")` class | `NullPointerException` — a body that dereferences the name | DIED |
| M11 | its message | `"argument can not be null"` — **the shared constant, getValue-style** | DIED |
| M12 | `putValue("k",null)` class | `none` — a guard that checks only the NAME | DIED |
| M13 | its message | `"argument can not be null"` | DIED |
| M14 | `putValue(null,null)` message | `"argument can not be null"` | DIED |
| M15 | `getValue(null)` class | `none` — a lookup that simply misses | DIED |
| M16 | its message | `"arguments can not be null"` — **the shared constant, putValue-style** | DIED |
| M17 | `removeValue(null)` class | `none` | DIED |
| M18 | its message | `"arguments can not be null"` | DIED |
| M19 | `putValue` ok | `AbstractMethodError` — the unregistered door, pre-F18 state | DIED |
| M20 | round trip | `null` — a `putValue` that silently no-ops | DIED |
| M21 | value class | `java.util.HashMap` — **E31-1 §2's map through the value door** | DIED |
| M22 | `getValueNames` after put | `[]` | DIED |
| M23 | absent name is `null` | non-null — a `getValue` that fabricates | DIED |
| M24 | `removeValue(absent)` | `IllegalArgumentException` — refusing unknown names | DIED |
| M25 | `removeValue(present)` | `AbstractMethodError` | DIED |
| M26 | `getValueNames` after remove | `[cratonvm.f36]` — a `removeValue` that no-ops | DIED |

### 6.2 `RSslLiveSession` — **95 of 95 died. NONE SURVIVED.**

All 95 rows, one mutant each, each mutant being what a **named** wrong
implementation answers. The substitution is applied inside `ck()` in a **copy** of
the fixture, keyed by row name and selected per run with `-Dmut=<row>`, so one
compile serves every mutant and the replacing value is the one named in the table
— never a generic flip. The table is `scratchpad/f36/live_mut.py`.

**The harness was validated AS a harness**, because a mutation run that cannot
kill is worth exactly as much as a guard that cannot fire, and this directory has
a record (`[proxy oracle]`) of an exhaustive sweep whose compared side was a
stand-in. Two rows — one `Boolean`-valued (`inv.sessionContext.isNull`) and one
`String`-valued (`server.peerPrincipal.message`) — were ALSO mutated by editing
the fixture's own source text, compiled and run. Both produce output
**byte-identical** to the hook-based mutant, exit code included:

```text
CK RSslLiveSession inv.sessionContext.isNull = true  WANT false
CK RSslLiveSession fails=1                                   rc=1   (both routes, diff empty)
CK RSslLiveSession server.peerPrincipal.message = peer not authenticated  WANT null
CK RSslLiveSession fails=1                                   rc=1   (both routes, diff empty)
```

That equivalence is not luck: `ck` is a pure comparison with no side effect and no
control flow depending on `want`, so substituting the expectation inside it and
substituting it in the source are the same program.

| family | rows | died | what the mutants name |
|---|---|---|---|
| `handshake` | 25 | **25** | the completed client session |
| `attrs` | 17 | **17** | the attribute map on a WIDE session, and the one-letter message pair |
| `distinct` | 6 | **6** | two connections, one SSLContext |
| `invalidate` | 14 | **14** | F18's headline - the half RSslNullSession cannot reach |
| `verifier` | 11 | **11** | the HostnameVerifier door, and the control that makes it mean something |
| `serverSide` | 15 | **15** | negotiated, peer NOT authenticated |
| `drainTrap` | 7 | **7** | HotSpot's KeepAliveCache recycle |

**`handshake`** — 25 rows, 25 died.

| row | mutated to (the implementation it names) | |
|---|---|---|
| `client.cipherSuite.isNullSentinel` | the SSL_NULL_WITH_NULL_NULL sentinel for a negotiation | DIED |
| `client.cipherSuite.matchesConnection` | two doors served by two registrars disagreeing | DIED |
| `client.conn.peerPrincipal` | the connection door refusing where the session answers | DIED |
| `client.conn.serverCertificates.length` | the connection door losing the chain its session has | DIED |
| `client.getId.length` | the null session's byte[0] for a real handshake | DIED |
| `client.getId.twiceEqualContent` | an id reseeded from a per-call session object | DIED |
| `client.getId.twiceSameArray` | an accessor handing out its own array, not a clone | DIED |
| `client.isValid` | the pre-F10 minter writing -1 into the stream-id slot | DIED |
| `client.leafSubject.equalsPeerPrincipal` | the same split seen from the certificate's side | DIED |
| `client.localCertificates` | an empty array where null is the contract | DIED |
| `client.localPrincipal` | the SERVER's identity reflected back at the client | DIED |
| `client.peerCertificates.length` | an empty chain where the peer sent one | DIED |
| `client.peerHost` | the peer ADDRESS reported as the requested host | DIED |
| `client.peerPort.isServerPort` | an unwritten port slot | DIED |
| `client.peerPrincipal.class` | the internal name type instead of the javax one | DIED |
| `client.peerPrincipal.equalsLeafSubject` | F10 N2: the two doors reading different tables | DIED |
| `client.peerPrincipal.name` | a fabricated subject | DIED |
| `client.protocol.isModernTls` | a fabricated protocol string outside JSSE vocabulary | DIED |
| `client.protocol.isNoneSentinel` | the NONE sentinel reported for a negotiation | DIED |
| `client.responseCode` | a server arm that never answered | DIED |
| `client.sessionContext.isNull` | no context minted for a live session | DIED |
| `client.sessionContext.isSSLSessionContext` | a fabricated object of the wrong type | DIED |
| `client.sslSession.isPresent` | getSSLSession() empty on a completed handshake | DIED |
| `client.sslSession.sameObjectTwice` | F10 N1: a fresh session minted per accessor call | DIED |
| `client.valueNames.length` | an attribute map pre-seeded by the implementation | DIED |

**`attrs`** — 17 rows, 17 died.

| row | mutated to (the implementation it names) | |
|---|---|---|
| `attrs.getValue` | a putValue that silently no-ops | DIED |
| `attrs.getValue.class` | E31-1 2: the attribute map through the value door | DIED |
| `attrs.getValue.null.message` | ONE shared constant, spelled putValue-style | DIED |
| `attrs.getValue.null.raises` | a lookup that simply misses and answers null | DIED |
| `attrs.putValue.nullName.message` | ONE shared constant, spelled getValue-style | DIED |
| `attrs.putValue.nullName.raises` | a body that dereferences the name | DIED |
| `attrs.putValue.nullValue.message` | the shared constant on the value arm | DIED |
| `attrs.putValue.nullValue.raises` | a guard that checks only the NAME | DIED |
| `attrs.putValue.raises` | the unregistered door, pre-F18 state | DIED |
| `attrs.removeValue.null.message` | the shared constant on the third site | DIED |
| `attrs.removeValue.null.raises` | removeValue treating null as nothing-to-remove | DIED |
| `attrs.removeValue.raises` | removeValue unregistered | DIED |
| `attrs.removed.valueNames` | a removeValue that no-ops | DIED |
| `attrs.shadow.peerHost` | E31-1 2: a width-blind slot-3 read on the WIDE shape | DIED |
| `attrs.shadow.peerPort.isServerPort` | the port slot lost once an attribute is set | DIED |
| `attrs.shadow.sessionContext.isNull` | the context lost once an attribute is set | DIED |
| `attrs.valueNames` | a putValue that stores nothing | DIED |

**`distinct`** — 6 rows, 6 died.

| row | mutated to (the implementation it names) | |
|---|---|---|
| `distinct.idsDiffer` | both ids empty, so equal for the wrong reason | DIED |
| `distinct.second.getId.length` | the second session falling back to byte[0] | DIED |
| `distinct.second.isValid` | only the first session marked negotiated | DIED |
| `distinct.second.peerPrincipal` | the peer chain recorded only for the first connection | DIED |
| `distinct.second.responseCode` | the second exchange never served | DIED |
| `distinct.second.sessionContext.isNull` | a context minted only for the first session | DIED |

**`invalidate`** — 14 rows, 14 died.

| row | mutated to (the implementation it names) | |
|---|---|---|
| `inv.before.sessionContext.isNull` | no context to drop, which would make the next row vacuous | DIED |
| `inv.cipherSuite.unchanged` | invalidate() treated as a reset | DIED |
| `inv.getId.length` | getId simplified onto the validity predicate | DIED |
| `inv.getId.unchanged` | an invalidate() that clears the id too | DIED |
| `inv.isValid` | a width-4 invalidate() that no-ops | DIED |
| `inv.other.sessionContext.isNull` | the whole context dropped for every session | DIED |
| `inv.other.stillValid` | invalidate() applied to the CONTEXT, not the session | DIED |
| `inv.peerCertificates.length` | the chain dropped on invalidate() | DIED |
| `inv.peerPrincipal.unchanged` | the peer chain dropped with the context | DIED |
| `inv.protocol.unchanged` | invalidate() treated as a reset | DIED |
| `inv.raises` | invalidate() unregistered in real-JDK mode, pre-F18 | DIED |
| `inv.sessionContext.isNull` | 'invalidate moves isValid and nothing else' - the claim F18 corrected | DIED |
| `inv.twice.isValid` | a second invalidate() flipping the bit back | DIED |
| `inv.twice.raises` | an invalidate() that is not idempotent | DIED |

**`verifier`** — 11 rows, 11 died.

| row | mutated to (the implementation it names) | |
|---|---|---|
| `verifier.cipherSuite.isNullSentinel` | the sentinel on the verifier's session | DIED |
| `verifier.control.notInvokedWhenBuiltInMatches` | a verifier called even when endpoint identification matched | DIED |
| `verifier.control.responseCode` | the control exchange never served | DIED |
| `verifier.getId.length` | the same minter's byte[0] | DIED |
| `verifier.hostArg` | the verifier handed the certificate name, not the requested host | DIED |
| `verifier.invoked` | the verifier never consulted - the null-capture that produced a wrong record | DIED |
| `verifier.isValid` | huc_verify_hostname's -1: the handshake it was invoked to vet | DIED |
| `verifier.peerPrincipal` | the verifier's session with no peer chain | DIED |
| `verifier.responseCode` | the IP-literal exchange never served | DIED |
| `verifier.sameObjectAsGetSSLSession` | F18's recorded claim, measured false here | DIED |
| `verifier.sessionContext.isNull` | no context on the verifier's session | DIED |

**`serverSide`** — 15 rows, 15 died.

| row | mutated to (the implementation it names) | |
|---|---|---|
| `server.cipherSuite.isNullSentinel` | the sentinel on the acceptor's session | DIED |
| `server.error` | a server thread that failed silently | DIED |
| `server.getId.length` | the acceptor's byte[0] | DIED |
| `server.isValid` | the acceptor's session marked not-negotiated | DIED |
| `server.localCertificates.length` | null where the server has a chain | DIED |
| `server.localPrincipal` | the server's OWN identity refused | DIED |
| `server.localPrincipal.class` | the internal name type | DIED |
| `server.peerCertificates.message` | the no-argument constructor again | DIED |
| `server.peerCertificates.raises` | an unauthenticated peer's chain fabricated | DIED |
| `server.peerPort.isPositive` | an unwritten port slot reported as 0 | DIED |
| `server.peerPrincipal.message` | a refusal built with the no-argument constructor | DIED |
| `server.peerPrincipal.raises` | an unauthenticated peer ANSWERED instead of refused | DIED |
| `server.session.isNull` | no server-side session at all | DIED |
| `server.sessionContext.isNull` | no context on the server side | DIED |
| `server.valueNames.length` | a pre-seeded attribute map | DIED |

**`drainTrap`** — 7 rows, 7 died.

| row | mutated to (the implementation it names) | |
|---|---|---|
| `drain.body` | a body the client never received | DIED |
| `drain.conn.cipherSuite.message` | the right class carrying another explanation | DIED |
| `drain.conn.cipherSuite.raises` | a connection accessor still answering after the recycle | DIED |
| `drain.conn.sslSession.message` | the right class, wrong explanation | DIED |
| `drain.conn.sslSession.raises` | getSSLSession still answering after the recycle | DIED |
| `drain.session.getId.length` | the session's id cleared with the connection | DIED |
| `drain.session.isValid` | the SESSION invalidated by the connection's recycle | DIED |

### 6.3 Determinism

Three consecutive runs of the shipped vector on HotSpot, byte-identical:

```text
run1 rc=0 3s   run2 rc=0 4s   run3 rc=0 4s
md5  9d86efa94509b107fa91a73f0b89464f  (all three)
```

And the result that makes §4.1 safe to have landed: the **62 s** runs taken
before that fix carry the **same md5**. The change is timing-only; not one of the
95 answers moved.

`RJdkSecurity` is 3 s and unchanged in cost — its 26 new rows add no I/O.

---

## 7. Residuals — the honest list

1. **No CratonVM run of any kind.** Every mutation result and every `PASS` line
   here is HotSpot. What these fixtures report on CratonVM is unknown to this
   lane.
2. **`RSslLiveSession` does not run until NOMINATION 1 lands.** It is in
   `src/`, so `compile_suite` compiles it and `run.sh`'s list-hygiene check
   reports it as an unregistered vector — a COVERAGE WARNING today, FATAL under
   `STRICT_COVERAGE=1`, which this file's own documentation says CI should set.
3. **Rows most likely to be the first red on CratonVM**, so the next lane does
   not have to guess:
   * `client.sslSession.sameObjectTwice` — F10-1 NOMINATION 1 is **recorded and
     unfixed**. This row goes red **by design** until a session is cached per
     carrier.
   * `client.peerPrincipal.*` and both `equals` rows — F10-1 NOMINATION 2 is
     recorded and unfixed: `getPeerPrincipal` reads the socket registry, which has
     no entry for an HTTPS carrier, and throws `SSLPeerUnverifiedException` where
     HotSpot answers `CN=localhost`. Red by design.
   * `inv.*` — F10-1 NOMINATION 3 records that **no native registers
     `javax/net/ssl/SSLSession.invalidate` in real-JDK mode**; F18 registered four
     doors, and whether this one now answers on a session of this width is
     precisely what has never been executed.
   * `attrs.shadow.*` — E31-1 §2's width-blind slot 3, on the WIDE shape.
   * the whole vector, at `phase=handshake`, if the accept side cannot serve TLS.
     **The one-line discriminator:** a capability gap fails with
     `CK RSslLiveSession FAILED phase=handshake …` and **no** `handshake=25`
     line; a contract divergence prints all 25 rows and a non-zero `fails=`.
4. **The `drainTrap` family asserts HotSpot's `KeepAliveCache` behaviour**, which
   is an implementation detail of `AbstractDelegateHttpsURLConnection`, not a JSSE
   contract. A CratonVM divergence there is a weaker finding than the other six
   families — answering the session after a drain is arguably friendlier. It is
   asserted anyway because the harness diffs the output regardless, and a row
   with a comment attached is cheaper to adjudicate than a bare difference.
5. **The negotiated cipher-suite NAME is not asserted anywhere.** CratonVM's TLS
   is rustls and HotSpot's is JSSE; their preference orders may legitimately
   differ. What is asserted is everything a fabrication must survive: not the
   `SSL_NULL_WITH_NULL_NULL` sentinel (E12-1's finding run in reverse — there a
   *real* suite name stood in for "nothing negotiated"), and agreement between
   `HttpsURLConnection.getCipherSuite()` and `SSLSession.getCipherSuite()`, which
   are two doors and two registrars.
6. **`getApplicationBufferSize()` is deliberately absent.** F18 §8.3 records
   CratonVM's 16384 as a knowing under-report (HotSpot: 16676 negotiated, 16704
   null). A row would be red for a reason this file does not own. Stated so it is
   not "helpfully" added — F25 had to state the same thing.
7. **`server.getPeerHost()` is not asserted.** Measured `127.0.0.1` through the
   `SSLServerSocket` server and **`kubernetes.docker.internal`** through the
   `HttpsServer` draft on this same host — a reverse-DNS answer, i.e. host
   configuration leaking into a fixture. Named because it is exactly the kind of
   row that passes for months and then fails on one machine.
8. **This worktree is SHARED and sibling-lane files are dirty in it.** Neither of
   this lane's files was dirty when it opened except `RJdkSecurity.java`, which
   carried F25's landed work. Anyone committing must stage
   `regression-suite/src/RJdkSecurity.java`,
   `regression-suite/src/RSslLiveSession.java` and this record **by path** —
   never `-a`, never `git commit -am`.
9. **`RJdkSecurity` still prints no `fails=` line.** It throws on the first
   divergence instead, so `fails` would be `0` on every line it ever printed.
   Pre-existing; F25-1 §5.4 declined to change it and so does this lane, for the
   same reason: changing a fixture's reporting shape is a separate change from
   adding rows to it. `RSslLiveSession`, being new, uses the documented
   `fails=` spelling from `harness-guard.sh` rather than `RSslNullSession`'s
   `failures=`.

---

## NOMINATIONS

### NOMINATION 1 — `regression-suite/run.sh`: schedule `RSslLiveSession` (REQUIRED — the vector is inert without it)

**Not this lane's file.** `run.sh` was clean in this worktree when this lane
closed; re-check before applying, because it moves under you.

`CORE_CLASSES`, **line 164**, currently ends:

```
... RJdkIntrinsics3 RJdkBridge1 RSslNullSession"
```

Append one word, so it ends:

```
... RJdkIntrinsics3 RJdkBridge1 RSslNullSession RSslLiveSession"
```

**`CORE_CLASSES` and not `JDKONLY_CLASSES`**, for the reason `RSslNullSession` is
there: this asserts `--real-jdk` COMPATIBILITY behaviour — the JSSE session
contract — not `--jdk-only` POLICY. Under `CRATONVM_ARGS="--jdk-only"` every
scheduled class already receives the flag, so a second registration would run the
identical command twice.

**No other wiring is needed, and that was checked rather than assumed:** the
vector needs no `class_args`, no `class_cv_args` and no `class_cp_extra` entry;
it publishes a check count so it needs no `harness-uncounted.txt` row; and its
source names no runtime mode, so `harness_guard_nondiscriminating`'s ADD scan
does not require a `HARNESS_NONDISCRIMINATING` row for it.

### NOMINATION 2 — `RSslNullSession.java`: the third null-contract site, and the live/null cross-reference

**Not this lane's file.** Its `attributes()` arm asserts `putValue`/`getValue`
behaviour but nothing about `removeValue(null)`, which is a **third** site
carrying the singular string. Exact literal text, to be added in `attributes()`
immediately after the existing `attrs.removeValue.raises` row (each row moves that
arm's contribution to the class total by one, 89 → 91):

```java
        // The THIRD null site. putValue says "arguments can not be null" (plural) and
        // getValue says "argument can not be null" (singular); removeValue is the
        // singular one too, so a single shared constant is wrong in exactly one of the
        // three places. Measured on jdk-25.0.3+9 (F36-1 §5.3).
        ck("attrs.removeValue.null.raises", raised(() -> s.removeValue(null)),
                "java.lang.IllegalArgumentException");
        ck("attrs.removeValue.null.message", message(() -> s.removeValue(null)),
                "argument can not be null");
```

This assumes a `message(...)` helper alongside the existing `raised(...)`; if
that vector has none, the second row can be spelled with its own try/catch.

Separately, `RSslNullSession`'s header §"What this vector cannot prove" should
gain one sentence pointing at `RSslLiveSession` as the file that now covers the
two halves it names, so a reader does not go looking for a gap that has been
closed.

### NOMINATION 3 — `regression-suite/jdk-only-coverage.txt` §3 is measured false

**Not this lane's file.** It reads:

```
3. TLS loopback handshake.
   RJdkSecurity covers SSLContext/SSLEngine construction, protocol and cipher
   enumeration and SSLParameters, but not a real handshake: that needs a key
   store, and generating a self-signed certificate portably requires internal
   sun.security APIs. "TLS loopback where supported" is therefore covered only
   as far as the engine surface.
```

The second sentence is wrong (§3.1). Suggested replacement:

```
3. TLS loopback handshake.  CLOSED 2026-08-13 by RSslLiveSession.
   RJdkSecurity covers SSLContext/SSLEngine construction, protocol and cipher
   enumeration and SSLParameters. The real handshake is RSslLiveSession's:
   loopback SSLServerSocket plus HttpsURLConnection, 95 checks over the client
   session, the server session, the HostnameVerifier door and invalidate(). The
   claim that a portable self-signed certificate "requires internal sun.security
   APIs" was measured FALSE -- it is assembled as DER and signed with
   java.security.Signature, all public API, in-process, with no resource file
   and no expiry. See docs/known-issues/jdk-only/F36-1-*.md SS3.1.
```

`RJdkSecurity`'s own copy of the same claim has already been corrected in this
commit.

### NOMINATION 4 — `F18-1`'s §8.3(4) needs a correction note

**Not this lane's file.** §1 above measures the opposite of what it records, and
shows that its evidence was a verifier that was never invoked. The record should
carry a dated correction rather than being silently contradicted by a fixture,
because this directory's own `[triage=stale]` lesson is that a reader opening any
single record in this chain gets a picture a later one has already corrected.
The same sentence is worth adding to the SSL/TLS chain block F25 added to
`INDEX.md` — which is also not this lane's file, and which is now one record
longer again.

---

## How to verify

Cheapest first. Every CratonVM row is unmeasured.

1. **Oracle, both vectors:**
   ```
   javac -d regression-suite/build regression-suite/src/RJdkSecurity.java \
       regression-suite/src/RSslLiveSession.java
   java -cp regression-suite/build RJdkSecurity     # srArgKinds=52, PASS (149 checks)
   java -cp regression-suite/build RSslLiveSession  # fails=0, PASS (95 checks)
   ```
2. **The tripwires are real — CHECKED, both forms.** `if (n != 52)` → `53` throws
   `AssertionError: srArgKinds ran 52 checks, header says 53` from
   `RJdkSecurity.secureRandomArgumentKinds`, and no `PASS` line.
   `sectionEnd("verifier", 11)` → `12` prints

   ```text
   CK RSslLiveSession FAILED phase=verifier java.lang.AssertionError: block verifier ran 11 checks, header says 12
   ```

   and exits 1 — note that it goes through the vector's own `catch (Throwable)`,
   so a denominator slip is reported on the `CK` prefix rather than escaping as a
   bare stack trace.
3. **The watchdog is real — CHECKED.** With its sleep dropped to 1 ms the run
   prints `CK RSslLiveSession FAILED watchdog-90s phase=startup` and exits **4**.
   A watchdog that has never been shown to fire is the same species of defect as
   the hang it guards against.
4. **CratonVM**, once NOMINATION 1 lands: `--real-jdk` for `RSslLiveSession`,
   both modes for `RJdkSecurity`. Read §7.3 first — five groups of rows are the
   expected first reds and each names the record that would explain it.
5. **Re-run the oracle three times and diff.** The vector opens sockets; if its
   output is ever not byte-identical across runs, something per-run has leaked
   into a `CK` line and that is a fixture defect, not a VM one.
