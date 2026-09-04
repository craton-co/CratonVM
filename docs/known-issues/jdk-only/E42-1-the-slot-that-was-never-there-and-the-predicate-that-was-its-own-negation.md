# E42-1 — the attribute slot that was never there, the predicate that was its own negation, and the "defaults" set nobody had

**Status: FIXED-UNVERIFIED (`native-builtins/src/phases_late/ssl_security.rs`, this lane's file); NOMINATED (the rest).**
**Prov: HotSpot column MEAS (this host, `scratchpad/e42/`, JDK 25.0.3+9-LTS, three byte-identical runs per probe); `jdk25src` citations READ; CratonVM column PRED.**
**2026-08-13, lane E42.** Lands NOMINATIONS 1, 2 and 6 of
`E31-1-the-unregistered-door-and-the-slot-that-resurrects-a-fabrication.md`,
and E12-1's residual 5.

**This lane may not build or run the VM.** Every CratonVM "after" below is
**PREDICTED**. Nothing here was compiled. No `cargo`, no VM, no fixture.

---

> **VERIFIED AGAINST A BINARY 2026-09-03, and the BLOCKING co-requisite is
> proven to be guarded — not merely present.** This record's status was
> *"Nothing here was compiled. No `cargo`, no VM, no fixture."*
>
> ```text
> cargo test -p cratonvm-native-builtins new13_tests      31 passed; 0 failed
> ```
>
> **§0.1 is the interesting one.** It says `NEW13_SSL_SESS_FIELDS` goes 3 -> 4
> and that `t27_tls::session_has_negotiated`'s `_ => true` arm *"must merge with
> its `3 =>` arm in the same commit or the null session becomes valid again"* —
> and that the co-requisite *"is enforced by a unit test rather than by a
> comment"*. All three halves check out:
>
> ```text
> NEW13_SSL_SESS_FIELDS = 4                    ssl_security.rs:1885
> 3 | 4 => match ctx.get_field(this, 2) { … }  t27_tls.rs:19538   (merged)
> new13_tests::the_widened_null_session_is_still_not_negotiated   1 passed
> ```
>
> **And the guard was mutation-proven.** A test that passes is not yet a test
> that guards. Un-merging the arm — `3 | 4` back to `3`, exactly the regression
> the record warns about — turns it red:
>
> ```text
> the_widened_null_session_is_still_not_negotiated ... FAILED
>   panicked at native-builtins/src/phases_late/ssl_security.rs:9261
> ```
>
> The mutation was reverted and the tree left byte-identical (`git diff` on
> `t27_tls.rs` empty). So the record's own claim — that a regression here fails
> a unit test *"rather than `RSslNullSession` silently reporting a valid null
> session again"* — is demonstrated, in both directions.
>
> **What this does NOT verify.** The record's status is FIXED-UNVERIFIED for
> **this lane's file only** and NOMINATED for the rest; nothing here adjudicates
> a nomination. The HotSpot columns from `scratchpad/e42/` are the oracle and
> were not re-measured — and that scratchpad did not survive its session, so
> they cannot be. §0.2's "five `SSLSession` widths become three" is a source
> claim about shapes and was not re-counted.

## 0. Verdict

1. **The 3-field session now has an attribute slot, and the widening is purely
   additive — but it carries a BLOCKING co-requisite in a file this lane does
   not own, and that co-requisite is enforced by a unit test rather than by a
   comment.** `NEW13_SSL_SESS_FIELDS` goes 3 → 4, which is byte-identical to
   `SSLServerSocket.accept`'s existing 4-field shape, so no existing slot
   moves. `t27_tls::session_has_negotiated`'s `_ => true` arm must merge with
   its `3 =>` arm in the same commit or the null session becomes valid again.
   NOMINATION 1. §1, §2.
2. **Five `SSLSession` widths become three.** The bespoke 2-field shape
   `SSLEngine.getSession` minted was the null session with two slots missing;
   it is now the same constructor. Widths 4, 6, 8 remain. §2.
3. **`isConnected()` was `!isClosed()` and that is wrong at BOTH ends, not
   one.** E31-1's NOMINATION 6 recorded the never-connected half. Measured
   here, a socket that was connected and then closed answers `isConnected() ==
   **true**` on HotSpot and `false` on CratonVM. The fix is a latch. §3.
4. **`isInputShutdown()`/`isOutputShutdown()` had the identical conflation, and
   the correct implementation already existed one module away.**
   `net_phase_e`'s `SockSide` has carried dedicated `input_shutdown` /
   `output_shutdown` flags all along and its own `java/net/Socket` natives both
   write and read them; the `SSLSocket` copies in this file asked `closed`
   instead. Measured: `close()` sets neither shut bit on HotSpot. §3.
5. **`isBound()` had no registration at all** and fell through to real
   `java.net.Socket` bytecode reading a `state` word out of a synthetic
   overlay — the exact shape `net_phase_e` recorded as "non-deterministically
   truthy roughly one run in three" when `isInputShutdown` had the same gap.
   §3.
6. **`SSLSocketFactory.getDefaultCipherSuites()` returned a narrower "defaults"
   set, and no such set exists.** Measured: default == supported == enabled,
   element-wise, 31 entries, one array. The 7-entry literal was not a stale
   copy — it modelled a distinction the oracle does not have, and it told a
   caller this VM cannot do ChaCha20 or any CBC suite. §4.
7. **`SSLEngine.getEnabledCipherSuites()` answered a ONE-element list**, and
   the comment justifying the gap said it was "already modeled by the
   getEnabledCipherSuites default branch above". Nothing was modelled: the
   branch was one hard-coded suite name. **The socket door in the same file
   already answered the full list** — two doors of one VM, 15 suites vs 1. §4.
8. **The fabrication sweep found no invented consumer in this file.** The
   species E31-1 found in `tls.rs` (a literal justified by a Keycloak
   heuristic that does not exist) has no analogue here: every consumer this
   file names — Jetty's `SecureRequestCustomizer`, Apache HttpClient5's
   `checkTLS`, Netty's `JdkSslContext$Defaults`, Tomcat's `SSLUtilBase` — is
   named for a *registration's existence*, not to justify a value. What it
   found instead is three **invented models** (§4) and one **invented
   negation** (§3). §5 states what was checked and cleared.

## 1. The co-requisite, and why it is a test and not a sentence

E31-1's NOMINATION 1 carries the ordering caution, and it is the whole
difficulty of this task:

> widening the 3-field `NEW13_SSL_SESS_FIELDS` moves it through
> `t27_tls::session_has_negotiated`'s arms. At 3 the arm tests `slot2 >= 0`; at
> 4 it falls into `_ => true` and **the null session becomes valid again**.

That is not avoidable by choosing a different width. Every arm of that function
was enumerated before anything was changed:

| width | `session_has_negotiated` | `session_cipher_slot` | `session_proto_slot` | `sslsess_attrs_slot` |
|---|---|---|---|---|
| 0–2 | `false` | 1 (0–1: `None`) | 0 (0–1: `None`) | `None` |
| 3 | slot 2 `>= 0` | 1 | 0 | `None` |
| 4 | **`true`** | 1 | 0 | `Some(3)` |
| 5 | **`true`** | 1 | 0 | `None` |
| 6 | **`true`** | 0 | 1 | `None` |
| ≥ 7 | slot 2 `!= 0` | 0 | 1 | `Some(n-1)` |

There is **no width ≥ 4 whose arm reads slot 2 as a stream id**, so no
unilateral widening exists. The two candidate destinations are:

* **4** — one arm to change, and the change is a *provable no-op* for the only
  other shape at that width. `SSLServerSocket.accept`'s slot 2 is
  `RUSTLS_SOCK_ID_BASE + stream_id` (`t27_tls.rs`, `let tls_id =
  crate::servlet::RUSTLS_SOCK_ID_BASE + stream_id;`), so `>= 0` is true for
  every session it has ever minted, which is exactly what `_ => true` answered.
  Nothing else moves: slots 0, 1 and 2 keep their meanings and their readers.
* **9** — needs no `t27_tls` change at all (it lands on the `≥ 7` arm with slot
  2 as a validity flag), but it **shifts three existing slots**: cipher 1 → 0,
  protocol 0 → 1, stream id 2 → 6. That is the change the brief warns about,
  and it would have been made without a compiler.

**4 was chosen because it is additive and 9 is not.** The cost is a one-line
change in a file this lane does not own, so the cost is paid by making the
build fail rather than by asking:

```rust
// native-builtins/src/phases_late/ssl_security.rs, test module
#[test]
fn the_widened_null_session_is_still_not_negotiated() {
    let sess = ctx.alloc_object(ClassId::new(0), NEW13_SSL_SESS_FIELDS);
    ctx.set_field(sess, NEW13_SESS_TLSID, Value::Int(-1));
    ctx.set_field(sess, NEW13_SESS_ATTRS, Value::Object(None));
    assert!(!crate::t27_tls::session_has_negotiated(&ctx, sess), ...);
}
```

`session_has_negotiated` is `pub(crate)`, so this file can call the other
file's predicate directly. If NOMINATION 1 does not land, `cargo test -p
cratonvm-native-builtins` is RED on this test — not `RSslNullSession` silently
regressing four checks that nobody has ever seen run. Its mutation check,
`the_widened_session_still_reports_a_real_stream_as_negotiated`, is what stops
the predicate being made unconditionally `false` to satisfy the first.

`the_attribute_slot_is_the_last_one_and_is_not_the_stream_id` pins the other
half of the agreement: `sslsess_attrs_slot` resolves the slot from the WIDTH,
so `NEW13_SESS_ATTRS` must be `NEW13_SSL_SESS_FIELDS - 1` and must not be
`NEW13_SESS_TLSID`. Those two assertions are the defect in one line each.

## 2. What the widening actually buys, and what it does not

**Buys.** `putValue`/`getValue`/`removeValue`/`getValueNames` round-trip on the
client `SSLSession` for the first time. Jetty's
`SecureRequestCustomizer.retrieveSni()` does `getValue()` then `putValue()` on
every SSL request; before E31 that `putValue` corrupted the stream id, after
E31 it was a silent no-op, and neither is what Jetty asked for.

**Retires a width.** `SSLEngine.getSession`'s 2-field fallback minted a shape
whose only distinction from the null session was the two slots it lacked. It is
the same object in the same state, so it is now
`new13_alloc_null_ssl_session(-1)`. That also gives it the GC pinning it never
had — it created two Strings across an unpinned session reference — and pins
the receiving engine across the constructor, which the old site also did not.

**Does NOT buy a stable creation time.** `t27_tls`'s
`getCreationTime`/`getLastAccessedTime` gate on `> 5` fields, so width 4 still
answers `epoch_millis_now()` on every call and two reads of one session still
disagree by the elapsed millis. E22-1's NOMINATION C asked for a widening that
carried a creation-time slot; **this is not that widening**, and saying so
matters because the two are easy to conflate. Reaching slot 5 means width ≥ 6,
which is `tls.rs`'s and `http2.rs`'s shape and lands on the `≥ 6` cipher/proto
convention — i.e. the three-slot shift rejected in §1.

**Does NOT change any answer for the two out-of-file minters.**
`http_url_connection.rs::huc_verify_hostname` and
`net_phase_e::https_session_object` both allocate with
`NEW13_SSL_SESS_FIELDS` and write only slots 0/1/2 through the named
constants, so they widen with it and their attribute slot is the allocation
default — which the attribute API's `Value::Object(Some(_))` match already
treats as "no attributes". Their comments still say "3-field"; NOMINATION 3.

**A pre-existing divergence those two carry, disclosed rather than fixed.**
Both write `NEW13_SESS_TLSID = -1` for a session whose handshake genuinely
*completed* (the connection owns its rustls state outside the id space, and
both say so). `session_has_negotiated` therefore answers `false` for them, so
an HTTPS session that really did negotiate reports `isValid() == false` and
`getId() == byte[0]` where HotSpot reports `true` and 32 bytes. This change
does not move that answer either way — but note that landing the widening
*without* NOMINATION 1 would move it to `true`, i.e. accidentally right for
these two and wrong for the null session, which is a good reason to land the
arm for the stated reason rather than notice the accident.

## 3. The socket predicates — one negation, five states

`scratchpad/e42/E42SocketPredicates.java`, HotSpot 25.0.3+9-LTS, **three
byte-identical runs**. No probe state is inferred; every row is printed.

```
ARMA fresh-SSLSocket            isConnected=false isClosed=false isBound=false isInputShutdown=false isOutputShutdown=false
ARMB closed-never-connected     isConnected=false isClosed=true  isBound=false isInputShutdown=false isOutputShutdown=false
ARMC fresh-plain-Socket         isConnected=false isClosed=false isBound=false isInputShutdown=false isOutputShutdown=false
ARMD closed-never-connected-pln isConnected=false isClosed=true  isBound=false isInputShutdown=false isOutputShutdown=false
ARME connected-open             isConnected=true  isClosed=false isBound=true  isInputShutdown=false isOutputShutdown=false
ARMF after-shutdownOutput       isConnected=true  isClosed=false isBound=true  isInputShutdown=false isOutputShutdown=true
ARMG connected-then-closed      isConnected=true  isClosed=true  isBound=true  isInputShutdown=false isOutputShutdown=true
ARMH bound-not-connected        isConnected=false isClosed=false isBound=true  isInputShutdown=false isOutputShutdown=false
```

`ARMA`'s receiver is `sun.security.ssl.SSLSocketImpl` from the zero-arg
`SSLSocketFactory.getDefault().createSocket()` — the exact object
`RSslNullSession` DOOR 1 uses.

Confirmed from source rather than only measured
(`jdk25src/java.base/java/net/Socket.java:115-157`): these are five
**independent bits of one word** —

```java
private static final int BOUND     = 1 << 1;
private static final int CONNECTED = 1 << 2;
private static final int CLOSED    = 1 << 3;
private static final int SHUT_IN   = 1 << 9;
private static final int SHUT_OUT  = 1 << 10;
```

— each read by its own `(s & BIT) != 0`. **`close()` sets `CLOSED` and nothing
else.** So "not closed" is not an imprecise spelling of `isConnected()`; it is
a different question, and the answers coincide in none of the four states where
they could differ.

| accessor | CratonVM before | HotSpot (MEAS) | CratonVM after (PRED) |
|---|---|---|---|
| `isConnected()` fresh | **`true`** | `false` | `false` |
| `isConnected()` closed-never-connected | `false` | `false` | `false` |
| `isConnected()` connected, open | `true` | `true` | `true` |
| `isConnected()` connected-then-closed | **`false`** | `true` | `true` |
| `isClosed()` (all arms) | correct | — | **unchanged** |
| `isBound()` | *unregistered* — real bytecode over a synthetic overlay | see table | the connected latch |
| `isInputShutdown()` closed | **`true`** | `false` | `false` |
| `isOutputShutdown()` closed | **`true`** | `false` | `false` |
| `isInputShutdown()` live | `false` | `false` | `false` |

### The latch, and why it cannot make anything worse

This VM's `SSLSocket` has no `connected` bit, so `new13_socket_ever_connected`
reconstructs one from state that already exists and that `close()` already
leaves alone: a live stream id, else `NEW13_SOCK_HOST` holding a String, else
`net_phase_e`'s side-table `host`. The middle one is the latch — all three
connect paths write it (`connect`, `new13_finish_socket`, the layered
handshake), `SSLServerSocket.accept` writes the same slot, the zero-arg
`createSocket()` never does, and nothing clears it.

Relative to `!closed` this can only move a never-connected socket `true` →
`false` and a closed-with-a-known-peer socket `false` → `true`. Both are
corrections. A connected socket whose peer was recorded in neither place still
answers `false` after close — which is the answer it already gave. **There is
no input for which this introduces a new wrong answer.**

### `isInputShutdown` — the twin that had already been fixed elsewhere

The comment that stood on these two registrations was:

> There is no independent half-close state for a rustls SSLSocket: the only
> supported shutdown operation is `close()`, which marks the shared side-table
> entry closed. Use that authoritative state for both directions rather than
> interpreting the host JDK's physical layout.

Both clauses are false. `net_phase_e::SockSide` declares `input_shutdown` and
`output_shutdown`; its `java/net/Socket.shutdownInput()`/`shutdownOutput()`
natives *set* them (`sock_set(ctx, this, |s| s.input_shutdown = 1)`), and its
own `isInputShutdown`/`isOutputShutdown` *read* them. The state exists and the
correct readers exist; these two copies asked a different field.

**The consumer the old spelling was written for is unaffected**, which is the
check that made this safe to change without a run: HttpClient5's
`DefaultBHttpClientConnection$1.checkTLS()` calls `isInputShutdown()` before
every write on a **live** socket. A live socket is not closed and has not been
shut down, so it answered `false` before and answers `false` now. The only
behaviour delta is on a **closed** socket, which stops claiming a half-close
that never happened.

## 4. The fabrications — three invented models, one measured each

The brief's distinction is the operative one: a **sentinel** is a value the
caller can always tell from success; a **plausible fabrication** is one it
cannot. §4's three findings are a third species — not a wrong *value* but a
wrong *model*, asserted in a comment and then implemented.

### (a) `SSLSocketFactory.getDefaultCipherSuites()` — a "defaults" subset

`scratchpad/e42/E42FactoryDefaults.java`:

```
FACTORY default.count           = 31
FACTORY supported.count         = 31
FACTORY default==supported      = true
FACTORY default[0]              = TLS_AES_256_GCM_SHA384
SOCKET enabled==factoryDefault  = true
SOCKET enabled==socketSupported = true
```

Element-wise equal. There is one array behind three accessors. CratonVM
returned a hand-picked 7-element subset — the GCM suites only — so a caller
that intersects its configuration against `getDefaultCipherSuites()`, which is
the accessor's entire purpose, was told this VM cannot do ChaCha20 or any CBC
suite. It can: `t27_tls::SUPPORTED_CIPHER_SUITE_NAMES` lists all of them and
`t27_tls_cbc` implements the CBC ones.

### (b) `SSLEngine.getEnabledCipherSuites()` — a one-element list, and the comment that called it a model

`scratchpad/e42/E42EnabledSets.java`:

```
ENGINE enabledCipherSuites.count   = 31
ENGINE supportedCipherSuites.count = 31
ENGINE enabled==supported          = true
ENGINE enabled[0]                  = TLS_AES_256_GCM_SHA384
```

The fallback returned `["TLS_AES_128_GCM_SHA256"]`. The neighbouring
`getSupportedCipherSuites` comment justified the difference between the two
accessors as *"this synthetic engine has no negotiated-state distinction
between 'supported' and 'enabled defaults' beyond what's already modeled by the
getEnabledCipherSuites default branch above"* — and that branch was a single
`create_string` of one literal. Its first clause is right for a reason it did
not give: **HotSpot has no such distinction either.**

The sharpest part is that the **socket door in the same file already knew**.
`SSLSocket.getEnabledCipherSuites` and `.getSupportedCipherSuites` both call
`ssl_sock_supported_cipher_suites`, which reads the shared constant. One VM,
two doors, 15 suites versus 1. Netty's `JdkSslContext$Defaults.init` validates
its configured list against an engine's suites on every server bootstrap.

### (c) three drifted copies of one list

`SUPPORTED_CIPHER_SUITE_NAMES` has 15 entries. The two 13-entry inline copies
in this file predate its `TLS_DHE_RSA_WITH_AES_{128,256}_GCM_SHA{256,384}` pair,
so this VM's own factory and engine disclaimed two suites its sockets
negotiate. All three copies (and the 7-entry one) are now
`jsse_supported_suite_name_array`, one function, and a unit test walks the four
registered doors and asserts they all answer the constant's length.

### (d) the enabled-protocol pair was spelled backwards

```
ENGINE enabledProtocols   = [TLSv1.3, TLSv1.2]
ENGINE supportedProtocols = [TLSv1.3, TLSv1.2, TLSv1.1, TLSv1, SSLv3, SSLv2Hello]
```

`SSLEngine.getEnabledProtocols`'s default was `["TLSv1.2", "TLSv1.3"]`, under
the comment `// Default: TLSv1.2, TLSv1.3`. The order is directly observable —
`RSslNullSession` asserts the literal string `[TLSv1.3, TLSv1.2]` — and a
caller taking element 0 as "the version we would prefer" reads the reversed
list as a downgrade. Now `JSSE_ENABLED_PROTOCOLS`, one constant, pinned by a
test that carries the transcript.

**`getSupportedProtocols` deliberately did NOT become that constant.** Measured,
HotSpot's supported list is strictly wider than its enabled one, so these two
*are* a real distinction — unlike the cipher pair. This VM offers only the two,
so the lists coincide here **by capability rather than by definition**, and
collapsing them into one constant would erase the difference for whoever widens
the VM. That is the same "any fix that treats them as one family is wrong"
caution E12-1 §0 opens with, applied in the direction that says *don't*.

### (e) `getSSLParameters().getProtocols()` — E12-1's residual 5, closed

It built a one-element array from the **session's negotiated protocol**,
falling back to a hard-coded `"TLSv1.3"`. Both halves are the category error
E12 fixed in `getEnabledProtocols` next door: `SSLParameters` describes
CONFIGURATION, and the fallback announced this VM's preferred version as though
it were an outcome. Measured:

```
SOCKET getSSLParameters.protocols   = [TLSv1.3, TLSv1.2]
SOCKET getEnabledProtocols          = [TLSv1.3, TLSv1.2]
SOCKET params.protocols==enabled    = true
PINNED12 getEnabledProtocols        = [TLSv1.2]
PINNED12 getSSLParameters.protocols = [TLSv1.2]
```

The same answer through two doors, including under a version pin, so they are
now one call (`ssl_sock_enabled_protocols`). That also carries E12's sentinel
filter into this door for free: reading `NEW13_SESS_PROTO` raw would have
reported `["NONE"]` for an unconnected socket once
`new13_resolve_socket_session` began returning the null session — **the exact
unmasking E12-1 §5 records for site 8, in the door it did not check.** E12-1
noted this site "cannot leak the sentinel" because it reads the raw
`NEW13_SOCK_SESSION` field, which is true and was one registration away from
not being.

## 5. What the sweep checked and CLEARED

Stated because "I found nothing" is only useful with a denominator.

| candidate | verdict |
|---|---|
| the `TLS_AES_*` literals in the supported/enabled **lists** | **correct** — these are configuration, not negotiation. The sentinel argument does not apply and must not be applied: `SSL_NULL_WITH_NULL_NULL` in a supported list is what would destroy it |
| `SSLContext.getDefault()`'s `"TLSv1.3"` protocol field | **cleared** — it is the context's *configured* protocol, and a "Default" context really is TLS 1.3 here |
| `new13_ctx_max_protocol`'s `"TLSv1.2" => Tlsv12` table | **cleared** — a parse of the caller's own string, not a stand-in |
| `JSSE_NULL_CIPHER_SUITE` / `JSSE_NULL_PROTOCOL` | **cleared, deliberately** — unofferable, and pinned by a test that asserts the sentinel is absent from `SUPPORTED_CIPHER_SUITE_NAMES` |
| every comment in this file naming Jetty / HttpClient5 / Netty / Tomcat / OkHttp / Spring | **cleared** — each names a caller whose *existence* motivates a registration, not a value's content. None asserts a downstream heuristic the way `tls.rs`'s Keycloak comment did. `grep -n "Keycloak\|keycloak\|heuristic"` over this file returns three hits, all about X.509 parsing and thread shutdown |
| `t27_tls`'s two native-TLS acceptor `"UNKNOWN"` arms | **NOT TOUCHED, and I agree with the judgement.** They are reached *by succeeding* — the stream is encrypted and a suite genuinely was negotiated, native-tls 0.2 just exposes no accessor. Writing `SSL_NULL_WITH_NULL_NULL` there would assert "no cipher" about a live encrypted connection: false in the *dangerous* direction, the same direction as the fabrication this family removed, merely inverted. `"UNKNOWN"` is loud rather than plausible, which is the correct trade when the truth is unavailable |
| `getApplicationBufferSize` = 16384 | **not touched** — E12-1 NOMINATION 6 / E31-1 NOMINATION D territory, values deliberately unchanged |

## 6. The fixture — which of `RSslNullSession`'s 47 checks this lane affects

`regression-suite/src/RSslNullSession.java`, 47 checks, PASS on HotSpot,
already registered in `run.sh`'s `CORE_CLASSES`. Today **1 of 47 executes**
(E31-1 §6); once E31's `SSLSocket.getHandshakeSession` registration lands, all
47 do.

**This lane's work is on the path of 32 of the 47** — and under
`--synthetic-jdk`, 45 of 47.

| group | count | this lane | PRED |
|---|---|---|---|
| `socket.isConnected` | **1** | **the answer changes** | **RED → GREEN.** The only check running today, and the only one this lane flips |
| `socket.*` null-session block | 13 | the receiver's WIDTH changes (3 → 4) | green — 8 of the 26 socket/socketClosed checks (`getId.isNull`, `getId.length`, `isValid`, `getId.stable`, ×2 blocks) depend on NOMINATION 1; the unit test is what keeps that from being discovered here |
| `socketClosed.*` null-session block | 13 | same | green |
| `socket.getEnabledProtocols` × 2 | 2 | the code path is now `ssl_sock_enabled_protocols` / `JSSE_ENABLED_PROTOCOLS` | green, **answer unchanged** (`[TLSv1.3, TLSv1.2]`) |
| `unofferable.*` × 3 | 3 | served by `jsse_supported_suite_name_array` instead of a 13-entry inline copy | green, **answer unchanged** — the two AES literals and the sentinel's absence are identical in the constant |
| `socket.getHandshakeSession` | 1 | not touched | `null` (E31's registration) |
| `engine.*` (DOOR 2) | 14 | not touched **in real-JDK mode** — `createSSLEngine` mints `sun/security/ssl/SSLEngineImpl`, so `t27_tls`'s 8-field shape answers. Under `--synthetic-jdk` the retired 2-field shape was theirs, so all 13 null-session checks move onto the widened shape | green |

So the honest statement is **not** "N checks flip". It is: **one check flips
red to green, thirty-one more are served by code this change rewrote without
moving their answers, and the eight that a mis-sequenced landing would break
are held by a unit test rather than by this document.**

## 7. Residuals

1. **Nothing here was built or run.** The Rust was reviewed by hand against the
   trait signatures in `native-api/src/registry.rs` (`get_field`/`set_field`/
   `set_array_element`/`object_num_fields` are `&self`; `alloc_object`/
   `new_ref_array`/`create_string`/`pin_native_root` are `&mut self`;
   `read_native_pin` is `&self` and returns the fallback on a mock) and against
   the identical shapes already compiling in the same file. The whole file
   passes a string/char/comment-aware bracket balance scan, and **the scanner
   was mutation-checked** — inserting one `}` after `NEW13_SESS_ATTRS` makes it
   report `MISMATCH at line 1526`.
2. **CRLF preserved — zero bare LF introduced.** `ssl_security.rs` is pure CRLF
   in the working tree: 8,237 CRLF / 8,237 LF before, 8,685 / 8,685 after, so
   `LF - CRLF == 0` on both sides. Every edit went through the editor rather
   than a script (this directory records batch-insert scripts corrupting CRLF),
   and the counts were re-checked after the last hunk. Note `git show HEAD:`
   reports 0 CRLF for the same file — git stores LF and the working tree is
   CRLF, so the count must be taken from the file on disk, not from git.
3. **`NEW13_SESS_ATTRS` is not written by the two out-of-file minters.** Their
   attribute slot holds the allocation default rather than an explicit
   `Object(None)`. Both the attribute API and `getValueNames` match on
   `Value::Object(Some(_))`, so either default reads as "no attributes" — but
   the three minters in this file now assert it and those two inherit it.
   NOMINATION 3 carries the comment fix; the write is optional.
4. **`new13_do_create_socket`'s session construction is unpinned** across two
   `create_string` calls, and my `NEW13_SESS_ATTRS` write joins the existing
   three field writes behind them. Pre-existing and unchanged in kind — the
   other two minters of this shape both pin — but flagged so it is not bisected
   onto this commit. It is a one-line fix and it is deliberately not in this
   hunk, because the hunk already carries a cross-file co-requisite.
5. **`getPeerPrincipal`'s `> NEW13_SESS_TLSID` width test is still there**
   (E22-1 NOMINATION B / E31-1 NOMINATION 4), and the widening makes the
   proposed fix *better* than when it was written: `== NEW13_SSL_SESS_FIELDS`
   is now `== 4`, which matches the accept shape too — and correctly, because
   slot 2 is a stream id on both. Not done here for a stated reason: it is the
   same block at **six** sites, five of which are live only under
   `--synthetic-jdk`, and an exact-width test becomes fragile if
   `try_alloc_concurrent_synthetic`'s `num_fields.max(real)` ever clamps this
   shape wider. Landing a second independent behaviour change in this hunk
   would make the first one un-bisectable.
6. **The engine door's DOOR-2 checks are untouched in the mode that matters.**
   The 2-field retirement is `--synthetic-jdk`-visible and real-JDK-invisible,
   because `net_phase_e::register_re6_ssl_context` re-registers
   `createSSLEngine` to allocate `sun/security/ssl/SSLEngineImpl`. Recorded
   because "I unified the engine's null session" reads as a real-mode change
   and is not one.
7. **`isBound()` shares the connected latch**, which is a statement about this
   VM's `SSLSocket` surface (no `bind` registration exists on the class), not
   about `java.net.Socket`. HotSpot's ARM H — bound, not connected — is
   unreachable here. If `SSLSocket.bind` is ever registered, `isBound` needs
   its own flag; the registration says so inline.

## How to verify

Cheapest first. Every CratonVM row is PREDICTED.

1. **`cargo test -p cratonvm-native-builtins new13_tests`** — five new tests, no
   VM, no network. `the_widened_null_session_is_still_not_negotiated` is the
   co-requisite guard; **run its mutation check the mutation way** — revert
   `t27_tls::session_has_negotiated`'s arm merge and it must go RED while
   `the_widened_session_still_reports_a_real_stream_as_negotiated` stays green.
   Then make `session_has_negotiated` `return false` and the second must go RED
   while the first stays green. If either mutation leaves both green, the pair
   is measuring one branch.
2. **`cargo test -p cratonvm-native-builtins --features synthetic-jdk`** — the
   2-field retirement and the six `ssl_security` session accessors are live
   only there.
3. **`--dump-native-registry`** (flags BEFORE `-cp`) — confirm
   `javax/net/ssl/SSLSocket.isBound` now appears with `owns_slot: true`, and
   that `net_phase_e`'s later registrations do not take
   `isConnected`/`isInputShutdown`/`isOutputShutdown` back. `net_phase_e`
   registers those on `java/net/Socket`, a *superclass*, so the more specific
   `javax/net/ssl/SSLSocket` registrations should win — but that is an argument
   from the dispatcher and the dump is the authority.
4. **`bash regression-suite/run.sh` with `ONLY="RSslNullSession"`**, after
   E31's `getHandshakeSession` registration. `socket.isConnected` is the one
   line this lane changes; §6 says which 31 others it merely re-routes. A red
   among those 31 means a re-route moved an answer it should not have.
5. **Any embedded-HTTPS fixture, and the H2 `TestSsl` cluster.** The suite-list
   changes are the widest-blast-radius edit here: three accessors that used to
   return 7 or 13 names now return 15, and a caller that intersects against
   them gets a *larger* set. That direction cannot make a handshake fail, but
   it can change which suite is chosen. `TestSsl.testClientInitiatedRenegotiation`
   and `connectWithSslBundleAndOptionsMismatch` are the two that pin
   suite/version behaviour.
6. **Anything that closes an `SSLSocket` and then asks about it.**
   `isConnected()` on a closed socket goes `false` → `true` and
   `isInputShutdown()`/`isOutputShutdown()` go `true` → `false`. Both are
   HotSpot's answers; both are behaviour changes on a path this lane cannot
   run. Connection-pool code (HttpClient5's `checkTLS`, Jetty's connection
   reuse) is where a difference would show.

---

## NOMINATION 1 — `native-builtins/src/t27_tls.rs`: `session_has_negotiated` must read slot 2 at width 4 (**BLOCKING co-requisite**)

**Do not land this lane's `ssl_security.rs` change without this one.** With it,
nothing changes for any shape that exists today. Without it, the null session
becomes valid again and `getId()` returns 32 fabricated bytes — the exact
defect E12-1 and E22-1 removed. `the_widened_null_session_is_still_not_negotiated`
fails the build in that state, so the two cannot silently part company, but a
reviewer splitting the commit should know which half is which.

This nomination is a **no-op on its own** and can therefore land first:
`SSLServerSocket.accept` is the only other 4-field minter and its slot 2 is
`crate::servlet::RUSTLS_SOCK_ID_BASE + stream_id`, so `>= 0` is true for every
session it has ever produced — which is what `_ => true` already answered.

REPLACE (`native-builtins/src/t27_tls.rs`, in `session_has_negotiated`):

```rust
        3 => match ctx.get_field(this, 2) {
            Value::Int(tls_id) => tls_id >= 0,
            // Defensive: `putValue` stores its attribute HashMap in slot
            // `num_fields - 1`, which on THIS shape is the stream id — see
            // E22-1's NOMINATION on the attribute-slot collision. If that has
            // happened the id is already gone; keep the pre-E12 answer rather
            // than inventing a new one from a clobbered slot.
            _ => true,
        },
        n if n >= 7 => matches!(ctx.get_field(this, 2), Value::Int(v) if v != 0),
        // 4- and 6-field shapes are only minted after a handshake.
        _ => true,
```

WITH:

```rust
        // E42: 3 and 4 are ONE arm. `ssl_security`'s NEW-13 shape widened from
        // 3 to 4 so that `putValue` has a slot of its own instead of writing a
        // HashMap over the stream id (see `NEW13_SSL_SESS_FIELDS`), and width 4
        // is byte-identical to `SSLServerSocket.accept`'s shape below: proto,
        // cipher, streamId, attrs.
        //
        // Merging them is a no-op for the accept shape and the fix for the
        // widened one. accept writes `RUSTLS_SOCK_ID_BASE + stream_id` into
        // slot 2, which is always `>= 0`, so `_ => true`'s answer for it is
        // unchanged — and that arm's premise ("4- and 6-field shapes are only
        // minted after a handshake") is now FALSE for width 4, which is
        // precisely why it cannot stay. `ssl_security`'s null session carries
        // `Int(-1)`.
        //
        // Pinned from the other side by
        // `phases_late::ssl_security::new13_tests
        // ::the_widened_null_session_is_still_not_negotiated`, which calls this
        // function directly: if this arm regresses, that test fails rather than
        // `RSslNullSession` silently reporting a valid null session again.
        3 | 4 => match ctx.get_field(this, 2) {
            Value::Int(tls_id) => tls_id >= 0,
            // Defensive: a non-`Int` here used to mean `putValue` had
            // overwritten the stream id — the collision the widening removes.
            // Kept because this predicate is also reached from `tls.rs` and
            // from any future minter that has not been audited: an unreadable
            // slot must not silently answer "never negotiated" for a session
            // that did.
            _ => true,
        },
        n if n >= 7 => matches!(ctx.get_field(this, 2), Value::Int(v) if v != 0),
        // The 6-field shape is only minted after a handshake.
        _ => true,
```

**Also update the width table in the same function's doc comment**, whose row 3
is now wrong: rows "3" and "4" merge into one row reading
*"| 4 | `ssl_security::new13_alloc_{,null_}ssl_session`; `SSLServerSocket.accept` in this file | the stream id | a stream id was recorded (`>= 0`); the NULL session carries `-1` |"*,
and the header line "FIVE different widths" becomes "THREE" (4, 6, 8).

## NOMINATION 2 — `t27_tls.rs`: `sslsess_attrs_slot`'s width table has a row that no longer exists

No code change — `4 => Some(3)` is already correct and is exactly the slot the
widened shape provides. But its doc table still lists a 3-field row
(*"| 3 | `ssl_security::NEW13_SESS_TLSID` | see below — the worst one |"*) and
its closing paragraph still says widening those shapes "is nominated, not done
here: the 3-field one is `ssl_security.rs`'s and cannot be widened without
moving `session_has_negotiated`'s arms in the same commit". That has now
happened; leaving the sentence makes the next reader look for an open
nomination that is closed.

REPLACE the table rows for widths 2 and 3 with a single note that the 2- and
3-field shapes were retired by E42 (the 2-field engine fallback now calls
`new13_alloc_null_ssl_session`; the 3-field NEW-13 shape widened to 4), and
replace the closing paragraph's last two sentences with a pointer to
`docs/known-issues/jdk-only/E42-1-*.md`. The `_ => None` arm stays: `http2.rs`'s
and `tls.rs`'s 6-field shape still has no attribute slot, and it is now the
**only** shape that does not.

## NOMINATION 3 — `http_url_connection.rs` and `net_phase_e.rs`: two comments that say "3-field"

Both allocate with `NEW13_SSL_SESS_FIELDS` and so widen correctly with no code
change. Only the prose is stale.

`native-builtins/src/http_url_connection.rs`, REPLACE:

```rust
    // The 3-field client `SSLSession` shape (`new13_alloc_ssl_session`'s), so
```

WITH:

```rust
    // The client `SSLSession` shape (`new13_alloc_ssl_session`'s — 4 fields
    // since E42; the width is `NEW13_SSL_SESS_FIELDS` and must stay that
    // constant, because `t27_tls`'s slot rules are keyed on it), so
```

`native-builtins/src/tls.rs` carries three more, at roughly `:1099`, `:1141`
and `:3263`, each naming "the 3-field `new13_alloc_null_ssl_session` shape".
All three are reasoning about slot 2 on that shape, which is still the stream
id — so the reasoning survives and only the number is wrong. Change "3-field"
to "4-field" in each, or drop the number and name the constant.

## NOMINATION 4 — `net_phase_e.rs`: `java/net/Socket.isConnected()` has the other half of the same bug, and `isBound` is unregistered there too

`net_phase_e.rs`, `r.register(sock, "isConnected", "()Z", ...)`:

```rust
        Ok(Some(Value::Int(if s.stream_id >= 0 && s.closed == 0 { 1 } else { 0 })))
```

This is right for the never-connected socket (`stream_id == -1`) and **wrong
for the connected-then-closed one**: measured ARM G, HotSpot answers
`isConnected() == true` for a socket that was connected and then closed, and
`close()` here sets `closed = 1` *and* `stream_id = -1`
(`sock_mark_closed_for_upcall`), so this answers `false` twice over. The plain
`java.net.Socket` door needs the same latch the `SSLSocket` door just got. The
cheapest form is a `connected: i32` field on `SockSide`, set beside
`stream_id` in `sock_set_for_create_with_local_port` and never cleared:

```rust
    sock_set(ctx, this, |s| {
        s.port = port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
        // E42: a LATCH. `java.net.Socket.isConnected()` is "has this socket
        // ever been successfully connected" (`CONNECTED = 1 << 2`,
        // jdk25src/java.base/java/net/Socket.java:117) and `close()` does not
        // clear it — measured, HotSpot 25.0.3+9-LTS: a connected socket that
        // is then closed answers isConnected()=true, isClosed()=true.
        // `stream_id` cannot serve as this signal because
        // `sock_mark_closed_for_upcall` resets it to -1.
        if stream_id >= 0 {
            s.connected = 1;
        }
    });
```

with `isConnected` reading `s.connected != 0`. **`sock_mark_closed_for_upcall`
must not touch it.**

Separately: **`java/net/Socket.isBound()` is registered nowhere** — only
`ServerSocket` and `DatagramSocket` have one. So a plain `new Socket()`'s
`isBound()` runs real JDK bytecode over a synthetic overlay, the same gap this
file's own `isInputShutdown` comment records as "non-deterministically truthy
roughly one run in three". Measured answers: `false` fresh, `true` once
connected or bound, `true` after close.

## NOMINATION 5 — `docs/known-issues/jdk-only/INDEX.md`: list this family

`INDEX.md` lists **none** of `E12-1`, `E22-1`, `E31-1` or this record, though
they are one continuous investigation across four lanes. New `.md` files under
this directory are this lane's to create; `INDEX.md` is not. Add all four,
adjacent and in order, so a reader who finds one finds the chain:

```
- E12-1-the-null-session-and-the-fabricated-cipher.md
- E22-1-the-null-session-in-the-registrar-that-actually-answers.md
- E31-1-the-unregistered-door-and-the-slot-that-resurrects-a-fabrication.md
- E42-1-the-slot-that-was-never-there-and-the-predicate-that-was-its-own-negation.md
```

(This re-raises E31-1's NOMINATION 7, which is still open.)

## NOMINATION 6 — `regression-suite/src/RSslNullSession.java`: three checks that would have caught §3, and a `putValue` round-trip that would catch §2

Not this lane's file. The vector asserts `socket.isConnected` and stops; the
other four predicates in that family are unasserted, and so is the attribute
API the widening exists for. Four lines, no network, in `unconnectedSocket()`:

```java
        ck("socket.isBound", s.isBound(), Boolean.FALSE);
        ck("socket.isInputShutdown", s.isInputShutdown(), Boolean.FALSE);
        ck("socket.isOutputShutdown", s.isOutputShutdown(), Boolean.FALSE);
```

and, in `nullSession(String door, SSLSession s)` after the `getValueNames`
check — the executable form of E31-1's NOMINATION 1 and this lane's §2:

```java
        // The attribute API must ROUND-TRIP, and it must not disturb anything
        // else. Before E42 the 3-field session had no attribute slot, so this
        // putValue wrote a HashMap over the stream id and flipped isValid()
        // back to true and getId() back to 32 fabricated bytes.
        s.putValue("cratonvm.e42", "v");
        ck(door + ".putValue.roundTrip", String.valueOf(s.getValue("cratonvm.e42")), "v");
        ck(door + ".putValue.keepsInvalid", s.isValid(), Boolean.FALSE);
        ck(door + ".putValue.keepsEmptyId", s.getId().length, 0);
        s.removeValue("cratonvm.e42");
        ck(door + ".removeValue", String.valueOf(s.getValue("cratonvm.e42")), "null");
```

**Read the ordering before landing this.** `putValue.roundTrip` is PREDICTED
RED for the `engine` door in real-JDK mode until `t27_tls`'s 8-field shape is
checked (it has a dedicated attrs slot, so it should pass — but that is
predicted, not measured), and the whole block is PREDICTED RED for any shape
still lacking a slot. It also grows the check count from 47 to 65, which moves
a number the harness guard reads. Land it after NOMINATION 1, and expect the
`getValueNames` check immediately above it to need re-reading: it asserts
`array[0]`, which is only true *before* a `putValue`.

## NOMINATION 7 — the "defaults are a narrower set" model exists in two more files, and the suite list has FIVE copies

§4(a) fixed one instance of an invented model. A census of `getDefaultCipherSuites`
across the tree finds two more, both outside this lane's file, both with the
same measured refutation (HotSpot: `default == supported`, element-wise, 31
entries — `scratchpad/e42/E42FactoryDefaults.java`):

* **`native-builtins/src/t27_tls.rs`**, `javax/net/ssl/SSLServerSocketFactory
  .getDefaultCipherSuites` — a **three**-element inline list
  (`TLS_AES_128_GCM_SHA256`, `TLS_AES_256_GCM_SHA384`,
  `TLS_CHACHA20_POLY1305_SHA256`). The same file's
  `SUPPORTED_CIPHER_SUITE_NAMES` has 15, and this registration sits ~2,200
  lines below it. Replace the literal with that constant.
* **`native-builtins/src/tls.rs`** (`--synthetic-jdk`),
  `javax/net/ssl/SSLSocketFactory.getDefaultCipherSuites` returns
  `TLS13_CIPHERS` while its `getSupportedCipherSuites` two registrations below
  returns `TLS13_CIPHERS ++ TLS12_CIPHERS`. Same invented subset, spelled as a
  deliberate-looking difference between two adjacent bodies. Both should
  answer the union.

**The generalisable rule, since this is the second lane to count copies of this
list.** `SUPPORTED_CIPHER_SUITE_NAMES` is documented as the single source of
truth and there were **five** other spellings of it — three in
`ssl_security.rs` (now one call), one in `t27_tls.rs`, one in `tls.rs`. Four
had drifted behind it. A constant is only a source of truth for the call sites
that read it, and nothing in the build says which those are. A source-witness
test in `native-builtins/tests/registry_contracts.rs` — scanning for a
string-array literal whose first element matches `^TLS_(AES|ECDHE|DHE|CHACHA)`
outside the constant's own declaration — would make the next copy fail the
build instead of the next lane's census.
