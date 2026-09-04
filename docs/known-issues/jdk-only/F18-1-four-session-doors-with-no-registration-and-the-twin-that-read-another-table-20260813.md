# F18-1 — four `SSLSession` doors with no registration at all, the twin that read a different table, and the bit that did not need a slot

**Status: FIXED-UNVERIFIED (`native-builtins/src/t27_tls.rs`, `native-builtins/src/tls.rs`, `native-builtins/src/phases_late/ssl_security.rs` — this lane's three files). No NOMINATIONS: every change landed in a file this lane owns.**
**Prov: HotSpot column MEAS (this host, `scratchpad/f18/F18SessionContract.java`, JDK 25.0.3+9-LTS `Microsoft-13877124`, loopback `HttpsServer` + `HttpsURLConnection` + unconnected `SSLSocket` + pre-handshake `SSLEngine`, three runs byte-identical modulo the epoch millis and the ephemeral port); `jdk25src` and tree citations READ; registry census taken by `grep` over `native-builtins/src/`, NOT from a `--dump-native-registry` run; CratonVM column PRED.**
**2026-08-13, lane F18.** Lands **NOMINATION 2** and **NOMINATION 3** of
`F10-1-the-two-minters-that-told-a-completed-handshake-it-never-happened.md`,
**NOMINATION 4** of `E31-1-the-unregistered-door-and-the-slot-that-resurrects-a-fabrication.md`
(which re-raised E22-1's NOMINATION B), and the accessor half of E12-1's
residual 4.

**This lane may not build or run the VM.** Every CratonVM "after" below is
**PREDICTED**. No `cargo`, no VM, no fixture. The Rust was parse-checked with
`rustfmt --edition 2021 --emit=stdout` on scratch copies of all three files,
and **the check was mutation-verified on each of the three** — see §8.1.

---

> **VERIFIED AGAINST A BINARY 2026-09-03.** Status was *"No `cargo`, no VM, no
> fixture"*, with the registry census taken *"by `grep` over
> `native-builtins/src/`, NOT from a `--dump-native-registry` run"*. It has now
> been taken from a `--dump-native-registry` run.
>
> ```text
> cargo test -p cratonvm-native-builtins t27_tls    97 passed; 0 failed; 1 ignored
> ```
>
> **§0.1's four doors are all registered, and in this lane's own file.** The
> record's claim was that `invalidate`, `getPeerHost`, `getPeerPort` and
> `getSessionContext` had *no real-JDK-mode registration anywhere in the crate*,
> and that the failure mode is `AbstractMethodError` rather than a wrong value.
> Compatible mode, from the dump:
>
> ```text
> invalidate         javax/net/ssl/SSLSession   owns_slot=True   t27_tls.rs:20096
> getPeerHost        javax/net/ssl/SSLSession   owns_slot=True   t27_tls.rs:20139
> getPeerPort        javax/net/ssl/SSLSession   owns_slot=True   t27_tls.rs:20191
> getSessionContext  javax/net/ssl/SSLSession   owns_slot=True   t27_tls.rs:20321
>
> 4/4 registered, none missing
> ```
>
> A registration on an INTERFACE is normally unreachable — no dispatch door asks
> an interface about an instance method. It is reachable here for the reason
> this family depends on: the carrier is MINTED under the literal name
> `javax/net/ssl/SSLSession` (`try_alloc_concurrent_synthetic(ctx,
> "javax/net/ssl/SSLSession", …)`), so the interface name IS the receiver's
> runtime class.
>
> **The twin in this record's title shows up in the dump, in the other mode.**
> Under `--synthetic-jdk`, `native-builtins/src/tls.rs` takes the slot from
> `t27_tls.rs` on three of the four:
>
> ```text
>                    compatible                --synthetic-jdk
> invalidate         t27_tls.rs   owns=True    tls.rs:1301  owns=True  (t27_tls owns=False)
> getPeerHost        t27_tls.rs   owns=True    tls.rs:1371  owns=True  (t27_tls owns=False)
> getPeerPort        t27_tls.rs   owns=True    tls.rs:1384  owns=True  (t27_tls owns=False)
> getSessionContext  t27_tls.rs   owns=True    t27_tls.rs   owns=True
> ```
>
> Recorded as a fact, not a verdict: `register()` is last-write-wins, so which
> file answers these three depends on the mode, and **this record's fix owns the
> slot only on the shipping arms.** Whether `tls.rs`'s three are equivalent is
> NOT adjudicated here.
>
> **What this does NOT verify.** The HotSpot column came from
> `scratchpad/f18/F18SessionContract.java`, which did not survive its session,
> so the oracle side is unre-measurable and was taken as given. The dump proves
> registration and ownership; it does not prove any of the four returns the
> right VALUE — no fixture exercised them here, and 4/4 registered is exactly
> the state that stops `AbstractMethodError` and says nothing else.

## 0. Verdict

1. **Four of `javax.net.ssl.SSLSession`'s twenty abstract methods had no
   real-JDK-mode registration anywhere in the crate**, and the failure mode of
   an absent registration on this interface is not a wrong value — it is
   `AbstractMethodError`, because the declaration has no Code attribute. The
   four: `invalidate`, `getPeerHost`, `getPeerPort`, `getSessionContext`.
   `getSessionContext` had **zero** registrations in *either* mode. §1.
2. **F10-1 NOMINATION 3 called the second half of the `invalidate()` fix
   blocked, and its premise was true while its conclusion was not.** Recording
   an `invalidated` bit does need a slot the width-4 shape does not have —
   *if the bit has to live in the object*. It does not. `t27_tls.rs` already
   carries two per-session facts in GC-stable, object-keyed side tables
   (`session_peer_certs_table`, `session_local_certs_table`); the bit is now a
   third. No width moves, so none of the five width tables that decide what a
   session's slots MEAN is disturbed — which is the whole reason F6-1 §4 and
   E42-1 §2 reject widening. §3.
3. **`getPeerPrincipal` and `getPeerCertificates` did not merely disagree —
   HotSpot's contract makes disagreement impossible by construction, and this
   lane measured that.** `getPeerPrincipal().equals(peerCerts[0]
   .getSubjectX500Principal())` is `true` on a completed handshake. So the two
   doors are not "similar accessors that should be kept in step"; one is
   *defined as a projection of the other*, and any implementation in which they
   can differ is wrong before you look at a single value. There is now one
   resolver, `t27_tls::peer_certs_for_session`, and all three registrations of
   the pair call it. §2.
4. **The same edit closes a cross-connection certificate read that was on
   record twice and had never been landed.** `getPeerPrincipal`'s width test was
   `> NEW13_SESS_TLSID` — "three or more fields" — and slot 2 is a stream id on
   only some widths. On the 8-field engine session it is the `isValid` FLAG, so
   the door looked up `servlet::s2_tls_peer_cert_chain_der(0)` or `(1)`. Id `1`
   is **the first id `s2_next_free_id` ever hands out** (`servlet.rs`, counter
   seeded at `1`), so this was not an unreachable index. E31-1 NOMINATION 4 /
   E22-1 NOMINATION B. §2.3.
5. **`invalidate()` moves a SECOND accessor, which F10 did not report and no
   record in this directory had noted: `getSessionContext()` drops from
   `SSLSessionContextImpl` to `null`.** Measured on a genuinely negotiated
   session. It costs nothing here — this VM answers `null` in every state — but
   the "invalidate() changes `isValid()` and nothing else" line, repeated in
   four comments across three files, was one accessor short. §4.
6. **The marker-collision invariant was re-verified against `servlet.rs`, not
   taken from F10's record, and this change makes it strictly *less* load-bearing
   than F10 left it.** §5.
7. **`RSslNullSession`: this lane predicts 0 of 47 checks move, and — unlike
   F10's zero — that is a gap rather than a deliverable.** The fixture asserts
   nothing about any of the four doors. §7.

---

## 1. The census — what had no registration, and why that is an `Error` and not a value

`javap javax.net.ssl.SSLSession` on the oracle JDK lists **20 abstract methods**
and one `default`. Cross-referenced against every `r.register(` on that class
name in `native-builtins/src/`:

| method | real-JDK mode | `--synthetic-jdk` |
|---|---|---|
| `getId`, `isValid`, `getCreationTime`, `getLastAccessedTime`, `getCipherSuite`, `getProtocol`, `getPacketBufferSize`, `getApplicationBufferSize`, `getValue`, `putValue`, `removeValue`, `getValueNames` | `t27_tls` | `tls.rs` (wins) |
| `getPeerCertificates` | `t27_tls` (wins over `ssl_security`) | — |
| `getPeerPrincipal`, `getLocalPrincipal`, `getLocalCertificates` | `ssl_security` (sole) | — |
| **`invalidate`** | **NONE** | `tls.rs` |
| **`getPeerHost`** | **NONE** | `tls.rs` |
| **`getPeerPort`** | **NONE** | `tls.rs` |
| **`getSessionContext`** | **NONE** | **NONE** |

The ownership rows are E22-1 §1's `--dump-native-registry` table, cited rather
than re-derived (this lane cannot run the VM). The four **NONE** rows are this
lane's own `grep`, and they are the kind of claim a grep can actually settle:
`"invalidate"`, `"getPeerHost"`, `"getPeerPort"` each return exactly one
registration in the crate, in `tls.rs`, whose registrar `register_tls_natives`
is reached only from `register_synthetic_overrides`
(`#[cfg(feature = "synthetic-jdk")]`); `"getSessionContext"` returns none.

**Why the distinction matters more than the count.** An unregistered method on
a *class* falls through to bytecode. `javax.net.ssl.SSLSession` is an
**interface**, and the VM's synthetic session objects carry it as their runtime
class, so a virtual call resolves to a declaration with no Code attribute. The
observable is `AbstractMethodError`, not a wrong answer — the same signature
E31-1 recorded for `SSLSocket.getHandshakeSession` and the reason
`RSslNullSession` used to abort on its second line.

**Why E12-1 did not catch two of these.** Its residual 4 says
"`getPeerHost()`/`getPeerPort()` on a handshaked session:
`build_synthetic_ssl_session` hardcodes `null` / `-1`". That is true and it is
about the **producer**. Nothing in that record asked whether the **accessor**
existed, and it did not. A residual written against a producer does not cover
its consumer.

---

## 2. The drifted twin — `getPeerPrincipal` vs `getPeerCertificates`

### 2.1 What HotSpot actually contracts

MEASURED (`scratchpad/f18/F18SessionContract.java`, ARM C, three runs):

```text
getPeerCertificates().length                                    = 1
getPeerCertificates()[0].getSubjectX500Principal().getName()    = CN=localhost,OU=F18,O=CratonVM,L=X,ST=Y,C=ZZ
getPeerPrincipal().getClass()                                   = javax.security.auth.x500.X500Principal
getPeerPrincipal().getName()                                    = CN=localhost,OU=F18,O=CratonVM,L=X,ST=Y,C=ZZ
getPeerPrincipal().toString()                                   = CN=localhost, OU=F18, O=CratonVM, L=X, ST=Y, C=ZZ
getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal()) = true
```

The last row is the finding. `getPeerPrincipal()` is not a sibling accessor
that happens to agree; it is the leaf certificate's subject, and JSSE's own
`SSLSessionImpl` computes it that way. Note also `getName()` (RFC 2253, no
spaces after the commas) and `toString()` (spaces) differ — CratonVM's
`javax/security/auth/x500/X500Principal` natives in `jca/x500.rs` store the
RFC 2253 canonical form and `toString` returns it unchanged, so **this VM's
`toString()` is missing the spaces**. Not changed here: it is `jca/x500.rs`,
not one of this lane's files, and no consumer this lane can name parses that
string. Recorded so the next reader does not "verify" the principal by
`toString`.

And for the never-authenticated peer, both doors refuse identically:

```text
javax.net.ssl.SSLPeerUnverifiedException: peer not authenticated
```

Exception **kind** and message, measured on the unconnected `SSLSocket`'s
session and on the pre-handshake `SSLEngine`'s. Both were already correct in
the tree; asserted here because an `IOException` whose *message* names the
right thing never matches the caller's `catch`.

### 2.2 What CratonVM did

| | source read | HTTPS client session | 4-field socket session |
|---|---|---|---|
| `t27_tls::getPeerCertificates` (LIVE) | `session_peer_certs_table`, keyed on the OBJECT | **has an entry** (`record_client_peer_chain`) | populated at construction |
| `ssl_security::getPeerPrincipal` (LIVE) | `s2_tls_peer_cert_chain_der(slot2)`, keyed on the STREAM ID | **no entry** — the connection was never a registered socket | present |

So on one object, in one call sequence, `getPeerCertificates()` returned the
chain and `getPeerPrincipal()` threw. F10-1 NOMINATION 2.

### 2.3 The second bug in the same three lines

`ssl_security`'s width test was `object_num_fields(this) > NEW13_SESS_TLSID`,
i.e. width ≥ 3. Slot 2 means different things at different widths — this is the
table `session_has_negotiated` already carried and neither peer-identity door
consulted:

| width | minted by | slot 2 | a stream id? |
|---|---|---|---|
| 4 | `ssl_security`'s NEW-13 shape; `SSLServerSocket.accept` | stream id, `-1`, or `HTTPS_CLIENT_SESSION_MARKER` | **yes** |
| 6 | `tls.rs::init_ssl_session_fields`; `http2.rs` | `tls.rs::SES_VALID` | no |
| 8 | `build_synthetic_ssl_session` | the `isValid` flag | no |

At width 8 the door therefore asked the socket registry for the peer chain of
socket `0` or socket `1`. **`servlet::s2_next_free_id` seeds its counter at
`1`**, so `1` is the first id the process hands out — a valid engine session
could be handed an unrelated connection's peer certificate chain. Recorded by
E22-1 (NOMINATION B) and re-raised with a second reason by E31-1
(NOMINATION 4); never landed until now.

### 2.4 The fix

`t27_tls::peer_certs_for_session` is the one resolver. Object-keyed table
first, then `session_stream_id` (the width table above, as a function) into the
socket registry. All three registrations of the pair call it:
`t27_tls::getPeerCertificates` (live in real mode),
`ssl_security::getPeerPrincipal` (live in real mode),
`ssl_security::getPeerCertificates` (loses its slot, updated anyway — a dead
twin reading a different source is how the live pair drifted apart).

**Deliberately NOT `ctx.invoke_virtual(this, "getPeerCertificates", ...)`,**
which is the shape F10-1 NOMINATION 2 suggested. That would build
`X509CertImpl` mirrors so that `getPeerPrincipal` could pull a subject string
back out of them. Sharing the DER resolver gives the identical
single-source-of-truth property — there is one function that decides, so the
two cannot see different chains — without the round trip.

**This is a strict widening for `getPeerCertificates`**: it keeps the
object-table answer it already gave and gains the stream-id fallback its
neighbour had. No input that used to yield a chain can now yield empty.

---

## 3. `invalidate()` — the registration, and the bit that did not need a slot

### 3.1 Measured on a session that genuinely negotiated

This is the first measurement of the contract on a real handshake rather than
on a null session; F10's change is what made that population reachable.
ARM C, three runs byte-identical:

| accessor | before `invalidate()` | after `invalidate()` |
|---|---|---|
| `isValid()` | `true` | **`false`** |
| `getSessionContext()` | `SSLSessionContextImpl` | **`null`** |
| `getId()` | `byte[32]` | `byte[32]`, **byte-for-byte identical** |
| `getCipherSuite()` | `TLS_AES_256_GCM_SHA384` | unchanged |
| `getProtocol()` | `TLSv1.3` | unchanged |
| `getPeerPrincipal()` | `CN=localhost,…` | unchanged |
| `getPeerCertificates().length` | `1` | unchanged |
| `getPeerHost()` / `getPeerPort()` | `localhost` / `<port>` | unchanged |
| `getCreationTime()` | fixed | unchanged |

`invalidate()` twice is idempotent. A **second, untouched** connection's
session is unaffected (ARM D) — the bit is per session, not per process.

**Two accessors move, not one.** F10 reported `isValid` only, which is what
E12-1 arm E and F6-1 §4 also say. `getSessionContext()` is the second, and
because it was never registered here nobody could have seen it. It costs
nothing today: this VM answers `null` in every state, so it is already correct
for the invalidated case and merely under-reports the live one.

### 3.2 Why a side table rather than a slot

F10-1 NOMINATION 3 split the fix into (1) register it and (2) make it mean
something at width 4, and called (2) blocked because the bit "needs a slot this
shape does not have", citing F6-1 §4 and E42-1 §2 against another widening.
Both citations are right about widening and neither is about the bit.

The width of a session shape is what **every** reader in `t27_tls.rs` uses to
decide what its slots mean — `session_cipher_slot`, `session_proto_slot`,
`sslsess_attrs_slot`, `session_stream_id`, `session_has_negotiated`. Widening
NEW-13 from 4 to 5 would move it under all five and re-open E42-1's BLOCKING
co-requisite. A side table keyed on `gc_stable_objref_key` changes no width and
therefore retargets no reader, and `t27_tls.rs` already holds two per-session
facts exactly this way.

`t27_tls::session_is_valid` is now the one composition of both halves —
`negotiated && !invalidated` — mirroring HotSpot's `isRejoinable()`
(`sessionId.length() != 0 && !invalidated && …`), and all three `isValid`
registrations in the tree call it.

### 3.3 Two properties that make this safe to land without a fixture

* **The table is empty until an application calls `invalidate()`.** In a
  process that never does, `session_is_valid` reduces to exactly the
  `session_has_negotiated` that shipped before F18. No answer changes.
* **Growth is bounded by `invalidate()` calls, not by session mints.** F10-1
  §8.5 records `session_peer_certs_table` growing once per *accessor call* on
  the HTTPS path. This table is written by one door only. It does inherit the
  32-bit key width — a birthday collision would report a live session as
  invalidated — which is the same exposure the two cert tables already carry
  and is not made worse by a table that is empty in the common case. The key
  width is one fix for all three and is not attempted here.
* **`session_mark_invalidated` declines to record for a session that
  negotiated nothing.** Not an optimisation: measured, `invalidate()` on the
  null session changes nothing, and since validity is `negotiated && !invalidated`
  the bit could not have changed an answer there either. So the gate costs no
  fidelity and keeps the table off the one door that mints sessions nobody
  handshaked.

### 3.4 `--synthetic-jdk` gets the same bit

`tls.rs`'s two `isValid` copies (on `javax/net/ssl/SSLSession` and on
`sun/security/ssl/SSLSessionImpl`) now call `session_is_valid`, and its
`invalidate` writes the shared table **as well as** the wide-shape `SES_VALID`
slot. The slot write is kept because other code in that file reads `SES_VALID`
directly; the two are composed, not chosen between. This closes NOMINATION 3's
second half in that mode too — at width 4 `tls.rs`'s `invalidate` used to
no-op, so `SSLServerSocket.accept`'s session stayed valid after being
invalidated.

No `invalidate` is registered on `sun/security/ssl/SSLSessionImpl` and none is
needed: the bit is keyed on the session **object**, not on the class name the
call was routed through.

---

## 4. `getPeerHost` / `getPeerPort` / `getSessionContext`

MEASURED, all three states:

| | unconnected `SSLSocket` / pre-handshake `SSLEngine` | completed HTTPS handshake | after `invalidate()` |
|---|---|---|---|
| `getPeerHost()` | `null` | `localhost` | `localhost` |
| `getPeerPort()` | `-1` | the server's port | unchanged |
| `getSessionContext()` | `null` | `SSLSessionContextImpl` | **`null`** |

HotSpot reports the host the caller **asked for**, not the certificate's
subject. Both are `localhost` in this harness; do not verify one against the
other.

**Width handling is the trap this family keeps falling into.** Slot 3 is the
peer host on the 6- and 8-field shapes and the **attribute map** on the
4-field one, so a width-blind read returns a `java.util.HashMap` through a
`()Ljava/lang/String;` descriptor as soon as anything has called `putValue` —
and Jetty's `SecureRequestCustomizer.retrieveSni()` does, on every SSL request.
That is E31-1 §2's recorded defect in `tls.rs`'s copies. The new registrations
use `session_cipher_slot`'s `>= 6` boundary, deliberately the same number, so
this file has one width line and not two. Below it the answer comes from
`session_stream_id` → `servlet::s2_tls_session_info`, whose third and fourth
components are the host and port a client actually dialled — the half E12-1's
residual 4 said was missing.

One detail worth its line: **`Int(0)` in the port slot means UNWRITTEN, not
"port zero"**. Both wide producers write `-1` explicitly for "no peer"
(`build_synthetic_ssl_session` slot 4; `tls.rs::init_ssl_session_fields`
`SES_PEER_PORT`), and `http2.rs`'s 6-field session writes no slot at all, so a
zero is the allocator's fill. A connected peer never reports port 0, so mapping
it to HotSpot's `-1` cannot mask a real answer — and `0` returned through `()I`
would be the *plausible* wrong value this directory keeps recording as worse
than a loud one.

**`getSessionContext()` answers `null`, and `null` is the interface's own
answer, not a stand-in for one:**

> "This context may be unavailable in some environments, in which case this
> method returns null."
> — `C:\craton\jdk25src/java.base/javax/net/ssl/SSLSession.java:77-84`

This VM has no `SSLSessionContext` — no session cache, no id-keyed lookup, no
timeout — so "unavailable" is a true statement about it. Contrast
`getCipherSuite`, where the honest value has to be a JSSE sentinel because the
method may not return null at all. The constant is right in three of HotSpot's
four measured states and under-reports the fourth.

---

## 5. The marker-collision invariant, re-checked

The brief required this and it is the one thing in this change that could hand
one connection **another connection's certificate chain**, so it was re-derived
from `servlet.rs` rather than read out of F10-1.

```text
net_phase_e::HTTPS_CLIENT_SESSION_MARKER   = 0x0800_0000
servlet::PENDING_CONNECT_SOCK_ID_BASE      = 0x1000_0000   (servlet.rs:2445)
servlet::PENDING_LAYERED_SOCK_ID_BASE      = 0x2000_0000   (servlet.rs:2425)
servlet::RUSTLS_SOCK_ID_BASE               = 0x4000_0000   (servlet.rs:2415)
servlet::s2_next_free_id                   counter seeded at 1 (servlet.rs:2128/2146)
```

The marker is strictly below all three bases and far above the counter, so
`s2_tls_peer_cert_chain_der(marker)` is a plain `HashMap` miss. Being below
`RUSTLS_SOCK_ID_BASE` also keeps it off that function's rustls redirect
(`servlet.rs:2780`), which would otherwise send it to a *different* table.
**The invariant holds after this change.**

**It is now less load-bearing than F10 left it, not more.** `peer_certs_for_session`
consults the object-keyed table **first**, and that is the table HTTPS client
sessions are registered in. The common HTTPS path therefore never reaches the
id lookup at all, so the marker's miss is a second-line guarantee rather than
the only one. It still has to hold — an HTTPS connection whose peer chain was
empty (`record_client_peer_chain` no-ops on empty) does reach it — which is why
it is now asserted by a unit test in `t27_tls.rs`
(`the_https_session_marker_can_never_name_a_real_socket`) instead of by a
comment. Moving any one of the four numbers fails a build.

The `session_stream_id` width table narrows the surface a second way: only
widths 3 and 4 read slot 2 as a registry key at all, so the two shapes that
were feeding the registry an `isValid` flag no longer reach it.

---

## 6. What changed, by file

**`native-builtins/src/t27_tls.rs`**

| # | site | change |
|---|---|---|
| 1 | new `session_stream_id` | the slot-2 width table, as a function |
| 2 | new `peer_certs_for_session` | the ONE peer-chain resolver |
| 3 | new `session_invalidated_table` / `session_mark_invalidated` / `session_is_valid` | the `invalidated` bit and the two-half predicate |
| 4 | `register_ssl_session_real::getPeerCertificates` | inline table lookup → the resolver |
| 5 | `register_ssl_session_real::isValid` | `session_has_negotiated` → `session_is_valid` |
| 6 | `register_ssl_session_real` | **new** `invalidate()V` |
| 7 | `register_ssl_session_real` | **new** `getPeerHost`, `getPeerPort` |
| 8 | `register_ssl_session_real` | **new** `getSessionContext` |
| 9 | `tests` | 12 new tests (§8.2) |

**`native-builtins/src/phases_late/ssl_security.rs`**

| # | site | change |
|---|---|---|
| 10 | `getPeerPrincipal` | slot-2-as-registry-key → the resolver |
| 11 | `getPeerCertificates` (the copy that loses its slot) | same |

**`native-builtins/src/tls.rs`**

| # | site | change |
|---|---|---|
| 12 | `SSLSession.isValid` | `session_has_negotiated` → `session_is_valid` |
| 13 | `SSLSession.invalidate` | also writes the shared bit; slot write kept |
| 14 | `SSLSessionImpl.isValid` | same as 12 — it *said* "same composition as the other copy" and then spelled it out again, which is how the twin drifted last time |

### 6.1 Predicted effect on a completed HTTPS handshake

HotSpot MEAS; both CratonVM columns PRED. "Before" is HEAD with F10's minter
fix in the tree.

| | HotSpot (MEAS) | CratonVM before (PRED) | CratonVM after (PRED) |
|---|---|---|---|
| `getPeerPrincipal()` | `CN=localhost,…` | **THREW `SSLPeerUnverifiedException`** | **`CN=localhost,…`** ✓ |
| `getPeerCertificates().length` | `1` | `1` | unchanged |
| `invalidate()` | moves `isValid` | **THREW `AbstractMethodError`** | **moves `isValid`** ✓ |
| `isValid()` after `invalidate()` | `false` | n/a (threw) | **`false`** ✓ |
| `getId()` after `invalidate()` | `byte[32]`, identical | n/a | **`byte[32]`, identical** ✓ |
| `getPeerHost()` | `localhost` | **THREW `AbstractMethodError`** | `null` — see below |
| `getPeerPort()` | the server port | **THREW `AbstractMethodError`** | `-1` — see below |
| `getSessionContext()` | `SSLSessionContextImpl` | **THREW `AbstractMethodError`** | **`null`** (legal) |

The `getPeerHost`/`getPeerPort` rows are honest about a residual: on **this**
path the session is the width-4 HTTPS shape whose slot 2 is the marker, which
`session_stream_id` accepts but which has no `s2_tls_session_info` entry — so
the accessors fall through to HotSpot's null-session answers. That is a
thrown `Error` becoming a legal-but-under-reported value, which is the
direction, not the destination. The producer half is E12-1's residual 4 and is
untouched. On the width-4 **socket** shape, whose slot 2 is a real registry id,
both report real values.

### 6.2 Predicted effect on a session that negotiated nothing

**Nothing moves**, and that is the deliverable rather than a gap:

| | HotSpot | before | after |
|---|---|---|---|
| `isValid()` | `false` | `false` | **`false`** |
| `getId().length` | `0` | `0` | **`0`** |
| `getCipherSuite()` | `SSL_NULL_WITH_NULL_NULL` | sentinel | unchanged |
| `getPeerPrincipal()` | THROWS `SSLPeerUnverifiedException` | THROWS | **THROWS** |
| `invalidate()` then all of the above | unchanged | n/a (threw) | **unchanged** |

Held by `invalidate_changes_nothing_on_a_session_that_negotiated_nothing`.

---

## 7. `RSslNullSession` — 0 of 47, and this time that is a gap

F10-1 §7 established the denominator and why *its* zero was correct. This
lane's zero is not the same claim.

The fixture asserts nothing about `invalidate`, `getPeerHost`, `getPeerPort`
or `getSessionContext`, and its two `getPeerPrincipal` checks are on the null
session, where the answer is the refusal — correct before and after. So no
check flips.

**Reachable is not the same as flipped, and this change moves neither.** What
it does change is that the four doors can now be *called* at all: a fixture
extended with `s.invalidate()` or `s.getPeerHost()` would today abort DOOR 1
with `AbstractMethodError` exactly the way it aborted on
`getHandshakeSession` before E31 registered it — and the 45 checks after that
line would never run. Extending it is the obvious next step and is **not
attempted here**: `regression-suite/src/` is not this lane's file.

The cheapest regression test this commit actually has is the negative one:
if any edit here loosened `session_has_negotiated`, the null session would
report `isValid()` and 32 fabricated bytes again. Four of the new unit tests
fail loudly in that case without needing a TLS peer.

---

## 8. Residuals

### 8.1 Nothing was built, type-checked or run

`rustfmt --edition 2021 --emit=stdout` on scratch copies proves all three files
**parse**; it does not prove they compile. The check was **mutation-verified on
each of the three** — introducing one syntax error per file makes rustfmt exit
non-zero and name the line (`t27_tls.rs`: unclosed delimiter; `ssl_security.rs`
and `tls.rs`: `expected ';', found keyword 'if'`). `tls.rs` additionally needs
an empty `tls_impl.rs` beside the copy, because it declares `pub mod tls_impl;`;
without it rustfmt fails for that reason and not for a syntax one, which is the
kind of false green worth naming.

Type risks, named rather than waved at:

* `std::collections::HashSet` is fully qualified at both use sites so no import
  line changes.
* `servlet::s2_tls_session_info` returns `(String, String, String, u16)`; the
  port is widened with `as i32`.
* `crate::servlet::{PENDING_CONNECT_SOCK_ID_BASE, PENDING_LAYERED_SOCK_ID_BASE,
  RUSTLS_SOCK_ID_BASE}` and `crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER`
  are all `pub(crate)` and read from the same crate.
* `peer_certs_for_session` and `session_stream_id` take `&dyn NativeContext`,
  matching `local_certs_for_session`'s existing signature, so
  `ssl_security`'s `|ctx, args|` closures pass `ctx` unchanged.

### 8.2 Tests added (12), and what each would catch

All in `t27_tls.rs`'s `tests` module, all using the existing
`session_registry()` helper so they exercise the **registered** natives rather
than the private bodies:

1. `the_four_unregistered_real_mode_session_doors_are_registered` — a census,
   not a behaviour test: a body cannot be tested until it exists.
2. `the_https_session_marker_can_never_name_a_real_socket` — §5's invariant.
3. `slot_two_is_only_a_stream_id_on_the_widths_where_it_is_one` — §2.3, both
   values of the 8-field `isValid` flag.
4. `invalidate_moves_is_valid_and_nothing_else` — including that the id keeps
   its exact bytes, which is what a plausible "simplification" of `getId` onto
   `session_is_valid` would break.
5. `invalidating_one_session_does_not_invalidate_another` — mutation check for
   4 in both directions (all-false, and mark-everything).
6. `invalidate_changes_nothing_on_a_session_that_negotiated_nothing`.
7. `get_session_context_answers_null_rather_than_fabricating_one`.
8. `peer_host_and_port_do_not_read_the_attribute_slot` — the Jetty state.
9. `peer_host_and_port_are_read_on_the_shapes_that_carry_them` — mutation check
   for 8; without it both accessors could return the sentinel for everything.
10. `an_unwritten_peer_host_slot_is_null_and_an_unwritten_port_is_minus_one`.
11. `the_object_keyed_chain_is_visible_to_the_one_resolver`.
12. `a_session_with_no_recorded_chain_resolves_to_empty` — mutation check for
    11; the empty case is what makes the two doors *refuse*, which is the
    behaviour `RSslNullSession` asserts.

**Test isolation was checked, not assumed.** These tests write process-global,
identity-keyed side tables, which is the classic parallel-test collision.
`MockNativeContext` already reserves a unique block of the pointer space per
instance (`next_ptr = 8 + seq * PTR_STRIDE`, `test_utils.rs:60/96`) for exactly
this reason, so distinct contexts cannot collide. That striding is `usize` and
`gc_stable_objref_key` truncates to `u32`, so a process creating more than
~4096 mock contexts would wrap — a pre-existing property of the two cert
tables, not introduced here.

### 8.3 Not done, and why

1. **`X500Principal.toString()` is missing RFC 1779's spaces** (§2.1).
   `jca/x500.rs` is not this lane's file, and no consumer this lane can name
   parses that string.
2. **The `getPeerHost`/`getPeerPort` producer for the HTTPS path** (§6.1).
   E12-1's residual 4; a behaviour change on a live connection this lane
   cannot run.
3. **`getApplicationBufferSize` = 16384** remains a deliberate under-report
   (measured: 16704 null / 16676 negotiated). Unchanged and re-measured here,
   which confirms E12-1 §1 and E31-1's derivation from `SSLSessionImpl.java:1297`.
4. **`getSSLSession()` mints a fresh session per accessor call.** MEASURED
   again here: HotSpot returns the *same object* from two calls
   (`o1.get() == o2.get()` is `true`). Also measured, and not previously
   recorded: the object handed to a `HostnameVerifier` is **a different object**
   from the one `getSSLSession()` returns, so "the verifier's session" and
   "the connection's session" are not one object even on HotSpot. F10-1
   NOMINATION 1; `net_phase_e.rs` is not this lane's file.

   > **CORRECTION 2026-08-13 (F36-1 §1) — the sentence above is WRONG.** The
   > verifier claim was measured against a `HostnameVerifier` that was **never
   > invoked**: `HttpsURLConnection` consults a custom verifier only *after*
   > its own endpoint-identification check fails, so the callback never ran and
   > the "different object" was an artefact of reading an unset field.
   > `RSslLiveSession`'s `verifier` family asserts the invocation **first** and
   > then measures: it is the **same object**. Both halves live in one run, so
   > the false negative reproduces on demand rather than being argued about.
   > The rest of point 4 stands — HotSpot does return the same object from two
   > `getSSLSession()` calls, and F10-1 NOMINATION 1 is unaffected.
   > This note exists because of this directory's own `[triage=stale]` lesson:
   > a reader opening one record in a chain must not get a picture a later
   > record has already refuted.
5. **`RSslNullSession` is not extended** to cover the four doors (§7);
   `regression-suite/src/` is not this lane's file.
6. **Two `MockNativeContext`-visible behaviours are asserted; the HTTPS path is
   not.** Everything in §6.1 is PREDICTED. The oracle harness
   (`scratchpad/f18/F18SessionContract.java`) runs against HotSpot only.

### 8.4 CRLF

All three files are 100% CRLF and were before. Verified on disk after every
hunk: `t27_tls.rs` 13,002 → 13,884; `tls.rs` 5,400 → 5,450;
`ssl_security.rs` 8,685 → 8,718 — **`total - CRLF == 0` on all three**, i.e.
zero bare LF introduced. Counts must be taken from the working tree; `git show
HEAD:` reports 0 CRLF for these files because git stores LF.

### 8.5 This worktree is SHARED

Sixteen sibling-lane files are dirty in it. Two of this lane's three files
(`t27_tls.rs`, `tls.rs`) were **already dirty** when this lane opened — from
earlier lanes in the same investigation, including the `3 | 4 =>` arm this lane
was asked to confirm (present, §9). Anyone committing this work must stage
`native-builtins/src/t27_tls.rs`, `native-builtins/src/tls.rs`,
`native-builtins/src/phases_late/ssl_security.rs` and this record **by path** —
never `-a`, never `git commit -am`.

---

## 9. The pre-flight check the brief asked for

`session_has_negotiated`'s width arm reads `3 | 4 =>` at
`native-builtins/src/t27_tls.rs`. **Present**, with E42's reasoning attached
and pinned from the other side by
`ssl_security::new13_tests::the_widened_null_session_is_still_not_negotiated`.
Nothing to land.

---

## How to verify

1. **Build.** Nothing here has been compiled.
2. **`cargo test -p cratonvm-native-builtins t27_tls`** — the 12 new tests, and
   in particular `the_https_session_marker_can_never_name_a_real_socket`, which
   is the safety invariant.
3. **`--dump-native-registry`** (flags BEFORE `-cp`) — confirm the four new
   rows on `javax/net/ssl/SSLSession` carry `owns_slot: true`, and that
   `t27_tls.rs` still owns `getPeerCertificates` and `isValid` while
   `ssl_security.rs` still owns `getPeerPrincipal`. If ownership has moved,
   both sides now read one resolver so the *answer* is unchanged — but the
   tests would be measuring whichever copy lost.
4. **`RSslNullSession`** — must still report `failures=0`. Any movement here
   means an edit landed in the wrong place (§6.2).
5. **Any embedded-HTTPS fixture** — re-run
   `scratchpad/f18/F18SessionContract.java`'s ARM C shape against CratonVM and
   diff against `run1.txt`. The rows that should move are
   `getPeerPrincipal`, `invalidate`, and the three doors that used to throw.
