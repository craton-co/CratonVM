# G44-1 — the session the verifier was handed, and the drain hook that already exists

**Status:** PARTIAL. **Before: MEASURED** on
`C:/craton/target-rel3/release/cratonvm.exe` (`9ae371468`), 2026-08-17 — all
seven vectors below, the 14 failing rows of `RSslLiveSession`, and the
native-registry ownership dump. **After: NOT MEASURED, and could not be.** This
lane's brief forbids `cargo build`, `check` and `test`, and `target-rel3`
predates `aed6a3b73`, so no binary containing either this lane's changes or the
per-carrier session cache they build on exists. Every "after" is **PREDICTED**
and labelled so at every occurrence.

What *is* measured beyond the before-state: the recycle state machine of §2,
extracted verbatim into a standalone `rustc --test` harness outside the crate
tree (`scratchpad/rs/recycle.rs`; `rustc`, never `cargo`, nothing written under
`target/`), where its six states pass. That is not the same as running it in the
crate and is not claimed to be.

**Owned files:** `native-builtins/src/http_url_connection.rs` and
`native-builtins/src/phases_late/ssl_security.rs`. Everything else is a
NOMINATION in §6.

---

> **VERIFIED AGAINST A BINARY 2026-09-04.** The status block reads *"After: NOT
> MEASURED, and could not be."* §5's registrar table has now been re-taken from
> a `--dump-native-registry` run on a binary built from this tree, and it is
> **unchanged again — only the line numbers moved**, which is exactly what §5
> itself found when it re-measured against `G35-1` §3 three weeks ago:
>
> ```text
>                        G44-1 §5 (9ae371468)            this tree (2026-09-04)
> getCipherSuite         net_phase_e:8439        false   net_phase_e:9656        false
>                        http_url_connection:445 TRUE    http_url_connection:665 TRUE
> getServerCertificates  net_phase_e:8447        false   net_phase_e:9669        false
>                        http_url_connection:405 TRUE    http_url_connection:625 TRUE
> getLocalCertificates   net_phase_e:8470        false   net_phase_e:9692        false
>                        http_url_connection:433 TRUE    http_url_connection:653 TRUE
> getPeerPrincipal       net_phase_e:8488        false   net_phase_e:9710        false
>                        http_url_connection:466 TRUE    http_url_connection:695 TRUE
> getLocalPrincipal      net_phase_e:8501        false   net_phase_e:9728        false
>                        http_url_connection:495 TRUE    http_url_connection:724 TRUE
> getSSLSession          net_phase_e:8519        TRUE    net_phase_e:9746        TRUE
> ```
>
> Five overwritten, one keeping its slot, same six rows, same directions. §5's
> decision — *"the registrar collapse: re-measured, and NOT taken"* — rests on
> that table, and the table has survived three weeks of churn intact. `G7-1`
> §5.1 states the same finding from the other side; both now hold on one dump.
>
> The vectors this record's family is measured by are green in both modes in a
> full 129-vector run: `RSslLiveSession` (95 checks), `RJdkX509Intercept`,
> `RJdkSecurity`, `RCrypto`, `RJdkNet`, `RJdkAsyncChannel`.
>
> **What this does NOT verify, and it is the substance of §§1-3.** The subject
> is *the session the verifier was handed*: §1's `huc_verify_hostname` minting
> its own session, §1a's row-by-row list of what N1 must not disturb, §2's
> `disconnect()` never tearing the connection down, §3's drain hook one file
> over. **None of that is exercised here.** An ownership dump says which
> function answers a call, not what it hands a hostname verifier, and no HTTPS
> fixture was run. §4's argument that widening the 4-field client session is the
> WRONG fix is untouched, as is §8, *"What this lane could not settle"*.

## 0. The headline

| | before (MEASURED, `9ae371468`) | after (PREDICTED) |
|---|---|---|
| `RSslLiveSession` | **95 checks, 14 failing rows** | **13**, and 12 once `aed6a3b73` is in a binary |
| `RSslNullSession` | PASS, 89 checks | unchanged |
| `RJdkNet` | PASS, 81 | unchanged |
| `RJdkAsyncChannel` | PASS, 141 | unchanged |
| `RJdkX509Intercept` | PASS, 26 | unchanged |
| `RCrypto` | PASS, 57 | unchanged |
| `RJdkSecurity` | PASS, 153 | unchanged |

**The predicted 14 → 8 did NOT hold, and the reason is worth more than the
prediction was.** G35-1 §6 forecast 13 with N1 and 8 with N2, on the premise
that `http_url_connection.rs` could call `forget_https_carrier_session` "when an
`https:` body hits EOF". There is no such point in this file: the `https:`
response body is read to completion inside `perform` and handed to Java whole,
as a `java/io/ByteArrayInputStream`, so nothing in this file ever observes the
application draining it. The EOF *is* observable — but from
`native-io/src/lib.rs`, which is not this lane's, and where the hook that makes
it reachable already exists in mirror image (§3, N2).

The 14 rows, MEASURED, and where each one now stands:

```text
client.sslSession.sameObjectTwice     aed6a3b73, not in this binary   -1 (PREDICTED)
verifier.sameObjectAsGetSSLSession    §1, this lane                   -1 (PREDICTED)
client.peerHost                       §4 — NOMINATION N3
client.peerPort.isServerPort          §4 — NOMINATION N3
attrs.shadow.peerHost                 §4 — NOMINATION N3
attrs.shadow.peerPort.isServerPort    §4 — NOMINATION N3
drain.conn.cipherSuite.raises         §3 — NOMINATION N2
drain.conn.cipherSuite.message        §3 — NOMINATION N2
drain.conn.sslSession.raises          §3 — NOMINATION N2
drain.conn.sslSession.message         §3 — NOMINATION N2
server.localPrincipal                 not investigated (G35-1 §4)
server.localPrincipal.class           not investigated
server.localCertificates.length       not investigated
server.peerPort.isPositive            not investigated
```

---

## 1. `huc_verify_hostname` minted its own session

MEASURED, `RSslLiveSession` on `9ae371468`:

```text
CK RSslLiveSession verifier.sameObjectAsGetSSLSession = false  WANT true
```

HotSpot hands `HostnameVerifier.verify` the same `SSLSession` object that
`HttpsURLConnection.getSSLSession()` returns afterwards. `huc_verify_hostname`
STEP 2 allocated its own — the same four `set_field` calls
`net_phase_e::https_session_object` makes, deliberately sharing
`HTTPS_CLIENT_SESSION_MARKER` so the two could not drift — and two minters
cannot produce one object however identical their writes are.

`aed6a3b73` exposed `net_phase_e::https_carrier_session_object(ctx, conn)`: the
one minter, behind the one per-carrier cache. This lane calls it.

**Why it is a fast path and not a race.** `record_https_carrier_session` runs at
STEP 0, *above* STEP 1's `if builtin.is_ok() { return Ok(()); }` early return —
which is the path every successful request takes, and which this file's existing
`the_session_capture_precedes_the_success_path_early_return` witness already
pins. So by the time control reaches STEP 2 the carrier always has an entry.

**The local mint was kept, as `huc_mint_verifier_session`, and that is not
timidity.** Two states still reach it: `connection` is `None` (how `perform`
calls this for a request with no `HttpsURLConnection` object behind it, and the
exposed function's documented non-error `None`), and the `SSLSession` allocation
being refused. Propagating instead would mean *not consulting an installed
`HostnameVerifier`* — a security change, in a lane fixing an identity row.

Two smaller things came with it:

1. **The refused-allocation exit now unpins.** The `map_err(..)?` it replaces
   returned straight out of the function with `verifier_pin` and `host_pin`
   still on the pin stack. Pre-existing, on the one path that already had
   nothing to hand back.
2. **The fast path allocates two fewer strings.** `protocol` and `cipher` are
   only turned into Java strings inside the fallback now, because the carrier's
   session already carries them.

**Fixed:** `verifier.sameObjectAsGetSSLSession`. **After: PREDICTED** — 1 row,
and only in a binary that also has `aed6a3b73`: the cache the exposed function
reads is what makes two calls agree, and this lane cannot supply it.

### 1a. What N1 must not disturb, checked row by row

`verifier.*` has eight rows that are GREEN today and are answered by the object
this change swaps. Both objects are the same width and carry the same four
slots, so each row reads the same source before and after:

| row | reads | same after N1? |
|---|---|---|
| `verifier.isValid` | slot 2 via `session_has_negotiated` | yes — both minters write `HTTPS_CLIENT_SESSION_MARKER` |
| `verifier.getId.length` | `gc_stable_objref_key(session)` | yes — 32 bytes either way; the *contents* now match `getSSLSession()`'s, which is the point |
| `verifier.peerPrincipal` | `record_client_peer_chain`'s object table | yes — `https_session_object` records the same chain |
| `verifier.cipherSuite.isNullSentinel` | slot `NEW13_SESS_CIPHER` | yes |
| `verifier.sessionContext.isNull` | `session_is_valid` + `object_num_fields >= 7` | yes — width 4 in both |
| `verifier.invoked` / `.hostArg` / `.control.*` | not the session | untouched |

---

## 2. `disconnect()` never tore the connection down

MEASURED on HotSpot (`G7-1` §1d, transcribed there from
`scratchpad/g7/TlsProbe.java`): after `disconnect()` **all six**
`HttpsURLConnection` session accessors throw `IllegalStateException: connection
not yet open` again — the same exception, with the same message, that a
never-handshaked connection throws. The message describes the state, not the
call order. CratonVM kept answering for the life of the carrier object.

`huc_disconnect` now calls a new `https_recycle_carrier`, which tears down both
tables — because the six accessors read from two of them (`G7-1` §5.1, and §5
below re-measures it): the five this file owns read `https_peer_info`, and
`getSSLSession` reads `net_phase_e`'s `https_carrier_sessions`.

Three things are load-bearing and each is a defect on its own if it is dropped:

1. **The row is FLAGGED, not removed.** `https_ensure_exchanged` treats "no
   entry" as "this connection never handshaked" and drives a *fresh HTTPS
   exchange*. Removing the row would make the very next accessor re-issue the
   request over the network, repopulate the table, and answer — a wrong answer
   that costs a second request to produce. Pinned by
   `the_lazy_exchange_guard_is_a_presence_test_not_a_liveness_test`, which
   fails if that guard is ever "made consistent" with the three accessors.
2. **`forget_https_carrier_session` is called unconditionally**, not only when
   this file's table has a row. The two populators disagree about when they
   fire: `record_https_peer_info` early-returns on an empty peer chain while
   `record_https_carrier_session` runs on every completed handshake, so an
   anonymous-suite exchange has a carrier session and no peer info. Gating the
   release on this table would leak exactly those.
3. **The refusal is the `IllegalStateException`, not
   `SSLPeerUnverifiedException`.** A recycled row is filtered out *before*
   `https_peer_chain_or_throw`'s match, so it lands on the "no entry" arm. The
   other arm would claim the connection is open and the peer anonymous.

This is also the **first call site `net_phase_e::forget_https_carrier_session`
has ever had**, and therefore the first time anything is removed from
`https_carrier_sessions` — which, since `aed6a3b73`, also holds a global root on
one `SSLSession` per row. Until now that table grew by one entry per HTTPS
carrier for the life of the process. `disconnect()` is not every connection, so
this narrows the leak rather than closing it; N2 closes it.

**Rows closed in `RSslLiveSession`: zero.** The vector never calls
`disconnect()`. Stated plainly because the temptation here was to recycle at
`getInputStream()` instead, which *would* have turned the four `drain.conn.*`
rows green — and would have been wrong: HotSpot's `KeepAliveStream` returns the
connection at EOF, not at hand-out, so the accessors answer for the whole window
in between. Four green rows bought with a new divergence nothing measures is the
shape this directory exists to refuse.

---

## 3. The drain hook exists, and it is one file over

`--dump-native-registry` under `--jdk-only` on `9ae371468`, MEASURED
2026-08-17:

```text
java/io/ByteArrayInputStream  read  ()I     owns_slot=false  native-builtins/src/lib.rs:18554
java/io/ByteArrayInputStream  read  ()I     owns_slot=TRUE   native-io/src/lib.rs:6861
java/io/ByteArrayInputStream  close ()V     owns_slot=false  native-builtins/src/lib.rs:18700
java/io/ByteArrayInputStream  close ()V     owns_slot=TRUE   native-io/src/lib.rs:6867
```

So the `https:` response body's EOF and close are BOTH already native, and both
are owned by `native-io`. `native_bais_read`'s `if pos >= count { -1 }` is the
EOF instant and `native_bais_close` is a bare `Ok(None) // no-op`.

That matters for a second reason: **no new registration is needed**, so
`regression-suite/bridge-ratchet.sh` — the L6 gate that fires on an
unadjudicated `Bridge` over concrete bytecode — does not move. Every
alternative this lane considered did add one (a `close()V` bridge from this
file, a dedicated response-stream class, a `SequenceInputStream` sentinel), and
each would have needed a baseline refresh under `scripts/baselines/`, which is
not this lane's either.

And the hook shape is already in the tree, pointing the other way:
`native-api/src/registry.rs` defines `BaosEvent` / `BaosEventHook` /
`install_baos_event_hook` / `dispatch_baos_event`, `native-io` dispatches from
its `ByteArrayOutputStream` write/flush/close bodies, and
`http_url_connection::register_http_url_connection_real` installs
`huc_live_baos_event` as the consumer — for the request side. The response side
wants the mirror image and nothing new. **N2.**

Everything downstream of that hook is already written and already active: the
consumer only has to call `https_recycle_carrier` (§2) for the carrier that
owns the stream.

---

## 4. The 4-field client session, and why WIDENING IT IS THE WRONG FIX

MEASURED, `9ae371468`:

```text
CK RSslLiveSession client.peerHost                    = null   WANT localhost
CK RSslLiveSession client.peerPort.isServerPort       = false  WANT true
CK RSslLiveSession attrs.shadow.peerHost              = null   WANT localhost
CK RSslLiveSession attrs.shadow.peerPort.isServerPort = false  WANT true
```

`t27_tls`'s `getPeerHost`/`getPeerPort` (the only registrations of either name —
`owns_slot=true`, `t27_tls.rs:18135` and `:18163`, confirmed in the same dump)
read slot 3 and slot 4 on shapes of width `>= 6`, and otherwise fall back to
`session_stream_id` → `servlet::s2_tls_session_info`. The HTTPS client session
is width 4 and its slot 2 is `HTTPS_CLIENT_SESSION_MARKER`, a value chosen
*precisely so that every socket-registry lookup misses* — see its own doc
comment, which explains why this connection cannot be given a real stream id
without leaking one registry entry per request. So the fallback cannot answer,
by design.

The brief's suggestion — widen `NEW13_SSL_SESS_FIELDS` — was checked against
every `t27_tls` reader that keys on width, and against the fact that the NULL
session is minted from the same constant. It does not work at any width:

| reader | at 4 (today) | at 6 | at 7 |
|---|---|---|---|
| `session_proto_slot` / `session_cipher_slot` | 0 / 1 | **1 / 0 — SWAPPED** | **1 / 0 — SWAPPED** |
| `sslsess_attrs_slot` | `Some(3)` | **`None`** — the attribute API silently becomes a no-op | `Some(6)` |
| `session_has_negotiated` | slot 2 `>= 0` | **`_ => true`** — the null session is valid again | slot 2 `!= 0` |
| `getSessionContext`'s `engine_shape` | not engine | not engine | **engine — and also demands membership in `negotiated_session_keys`, which only `engine_session_for` writes** |

Width 6 costs the whole `attrs` family and re-opens E42's defect with the sign
flipped, taking `RSslNullSession` with it. Width 7 buys the two endpoint slots
and loses the three `*.sessionContext.isNull` rows that are green today
(`client`, `attrs.shadow`, `verifier`). Both swap protocol and cipher for every
minter of the shape. A private width for the HTTPS minters alone would
re-introduce the fifth `SSLSession` width E42 retired.

**The fix that costs nothing is a side table keyed on the SESSION OBJECT**, the
shape `t27_tls::record_client_peer_chain` already uses to solve the identical
problem for the peer certificate chain on this exact shape. **N3.**

Landed here instead: the "DO NOT WIDEN" block on `NEW13_SSL_SESS_FIELDS`
carrying the table above, and a test —
`this_shapes_slot_map_is_the_one_t27_derives_from_its_width` — that asserts the
constant's slot map against `t27_tls::session_proto_slot` /
`session_cipher_slot` *as functions*, so a widening fails the build with the
reason attached rather than silently swapping two accessors.

---

## 5. The registrar collapse: re-measured, and NOT taken

`--dump-native-registry` under `--jdk-only` on `9ae371468` (the newest binary,
one commit newer than the dump G35-1 §3 took on `9964ca733`). Identical on both
carrier classes; **unchanged, only the line numbers moved**:

```text
getCipherSuite         owns_slot=false  net_phase_e.rs:8439
getCipherSuite         owns_slot=TRUE   http_url_connection.rs:445  overwrote=bridge
getServerCertificates  owns_slot=false  net_phase_e.rs:8447
getServerCertificates  owns_slot=TRUE   http_url_connection.rs:405  overwrote=bridge
getLocalCertificates   owns_slot=false  net_phase_e.rs:8470
getLocalCertificates   owns_slot=TRUE   http_url_connection.rs:433  overwrote=bridge
getPeerPrincipal       owns_slot=false  net_phase_e.rs:8488
getPeerPrincipal       owns_slot=TRUE   http_url_connection.rs:466  overwrote=bridge
getLocalPrincipal      owns_slot=false  net_phase_e.rs:8501
getLocalPrincipal      owns_slot=TRUE   http_url_connection.rs:495  overwrote=bridge
getSSLSession          owns_slot=TRUE   net_phase_e.rs:8519
```

`invocations` is `0` on every row and proves nothing — the probe makes no HTTPS
request. `owns_slot` settles it.

**G7-1 N3b (collapse the two same-named registrars) was NOT taken, and the
brief's "prove it safe" is why.** Taking it means registering `getSSLSession`
from this file, which — `register()` being last-write-wins with no unregister —
*deletes* net_phase_e's body the instant it is added. That body is the only one
either failing assertion in §1 goes through, and no lane forbidden to build can
prove a replacement equivalent to it. The failure mode is not "the collapse is
imperfect"; it is "the one live `getSSLSession` is gone and five accessors still
work, so nothing looks broken until a vector reads a session".

This file's existing test
`this_files_registrar_owns_five_of_the_six_https_session_accessors` asserts
`getSSLSession` is **not** registered here, precisely to make that mistake a
build failure. It is left standing and untouched. Re-raised as **N5** with the
one precondition that would make it decidable.

Nothing in this lane adds, moves or removes a registration.

---

## 6. NOMINATIONS

**N1 — `native-builtins/src/net_phase_e.rs`: `getSSLSession` needs the same
recycle check the other five now have. 0 rows today, 2 rows with N2.**
`https_recycle_carrier` (§2) evicts `https_carrier_sessions` so this accessor
already refuses after a `disconnect()`. But the check that DECIDES whether a
carrier has been recycled lives in this file's table, and `getSSLSession` cannot
see it, so any future recycle trigger that flags without evicting would leave
five accessors refusing and the sixth answering. **Change:** none while
recycling always evicts; stated so the next lane knows the invariant it must not
break. The clean form is N5.

**N2 — `native-api/src/registry.rs` + `native-io/src/lib.rs`: a `BaisEvent`
hook, the mirror of the `BaosEvent` one both files already carry. Closes the
four `drain.conn.*` rows and is the only unbounded-growth path left in
`https_carrier_sessions`.**
`native-api/src/registry.rs` already defines `BaosEvent` / `BaosEventHook` /
`install_baos_event_hook` / `dispatch_baos_event`; `native-io`'s
`native_baos_write`/`_flush`/`_close` dispatch through it and
`http_url_connection::register_http_url_connection_real` installs
`huc_live_baos_event` as the consumer. **Change:** add
`BaisEvent { Eof, Close }` beside it; dispatch `Eof` from `native_bais_read`'s
`if pos >= count` arm (and `native_bais_read_bytes`'s) and `Close` from
`native_bais_close`, which is a bare `Ok(None)` no-op today; register the
consumer from this lane's file, mapping the stream object back to its carrier
and calling `https_recycle_carrier`. MEASURED HotSpot contract: at body EOF the
connection returns to the `KeepAliveCache` and every CONNECTION-level accessor
throws `IllegalStateException: connection not yet open` again, while the SESSION
object the application holds stays valid — `drain.session.isValid` and
`drain.session.getId.length` are green today and must stay green, which is the
row that separates "recycled" from "destroyed". **This adds no registration**,
so `bridge-ratchet.sh` does not move; every design that did add one is rejected
in §3.

**N3 — `native-builtins/src/t27_tls.rs`: a peer-endpoint side table keyed on the
session object. 4 rows.**
Not a wider session shape — §4 enumerates why every width fails. **Change:**
`record_client_peer_endpoint(ctx, session, host, port)` and its reader, beside
`record_client_peer_chain` and keyed the same way; then two lines, one in
`getPeerHost` and one in `getPeerPort`, consulting it *before* the
`session_stream_id` fallback and after the `>= 6` slot branch. The writers are
one line each in `net_phase_e::https_session_object` (which has the carrier and
can read the URL's host and port) and, for the fallback path,
`http_url_connection::huc_mint_verifier_session`. MEASURED targets:
`getPeerHost() = localhost`, `getPeerPort() = <the server port>` — note HotSpot
reports the host the caller ASKED for, not the certificate's subject, and here
both happen to read `localhost`; do not verify one against the other.

**N4 — `native-builtins/src/net_phase_e.rs`, `https_session_object`: the two
pins are never released.** `conn_pin` and `session_pin` are pushed and the
function returns without `unpin_native_roots`. Every caller inside that file is
an accessor body that returns straight to Java, so the pin stack is unwound by
the VM; this lane's new call site sits under `huc_verify_hostname`'s
`verifier_pin`, so its `unpin_native_roots(verifier_pin)` truncates them too.
Correct today by construction at both call sites and by accident at neither.
**Change:** pin from a base and release it before every `Ok`/`Err`, the shape
the rest of the file uses.

**N5 — G7-1 N3b, re-raised with its precondition.** Collapsing the two
`register_https_session_accessors` and the two tables is still right, and §5
says why no lane that cannot build may do it. **Precondition:** the lane that
takes it must be able to run `RSslLiveSession` after the change, because the
only evidence that `getSSLSession`'s replacement body is equivalent is the
vector reading a session through it. Until then the five dead bodies in
`net_phase_e::register_https_session_accessors` stay where they are, and this
file's `this_files_registrar_owns_five_of_the_six_https_session_accessors` is
the guard that keeps the mistake from being made by accident.

**N6 — the four `server.*` rows** (`server.localPrincipal`,
`server.localPrincipal.class`, `server.localCertificates.length`,
`server.peerPort.isPositive`) are server-side sessions and were not investigated
by this lane either. Unchanged from G35-1 §4.

---

## 7. What changed, and how it is guarded

`native-builtins/src/http_url_connection.rs`:

| change | rows |
|---|---|
| `huc_verify_hostname` takes the carrier's session from `net_phase_e::https_carrier_session_object`; the private mint moves to `huc_mint_verifier_session` as the fallback and now unpins on its refusal exit | §1, 1 |
| `HttpsPeerInfo.recycled` + `https_recycle_carrier`, and the three readers (`https_has_session`, `https_peer_chain_or_throw`, `getCipherSuite`) filtering on it | §2, 0 in this vector |
| `huc_disconnect` recycles both tables — `forget_https_carrier_session`'s first call site, and `https_carrier_sessions`' first eviction | §2, leak |

`native-builtins/src/phases_late/ssl_security.rs`:

| change | rows |
|---|---|
| the "DO NOT WIDEN" block on `NEW13_SSL_SESS_FIELDS`, carrying §4's width table | §4, 0 — it prevents a regression rather than closing a row |

**Four new tests**, in the existing `#[cfg(test)]` modules:

* `the_verifier_is_handed_the_carriers_session_not_a_second_one` — a source
  witness over CODE lines only (this function's body is more comment than code,
  and a prose-blind assertion would be satisfied by prose): the carrier lookup
  is present and `try_alloc_concurrent_synthetic` is *absent* from
  `huc_verify_hostname`, so the private mint cannot creep back into the default
  path. One object versus two is only observable with a live TLS peer.
* `disconnect_recycles_both_https_session_tables` — behavioural, over
  `MockNativeContext`: the row survives flagged, the cipher is not forged away,
  `net_phase_e`'s carrier session is gone, a second recycle is a no-op (a
  connection disconnected twice must not release a global root twice), and
  re-recording a handshake clears the flag.
* `the_lazy_exchange_guard_is_a_presence_test_not_a_liveness_test` — a source
  witness on the one line that keeps a recycled carrier from re-issuing its
  HTTPS request. The dangerous edit here *looks like* consistency with the three
  accessors, which is why it needs a test and not a comment.
* `this_shapes_slot_map_is_the_one_t27_derives_from_its_width` — asserts the
  shape's slot map against `t27_tls::session_proto_slot`/`session_cipher_slot`
  as functions, so the widening §4 rejects fails the build.

The first three were **not** run inside the crate. The recycle state machine
behind the second was extracted verbatim into `scratchpad/rs/recycle.rs` and run
under `rustc --edition 2021 --test`: six states, all passing — never handshaked,
open, recycled (all five doors refusing), idempotent recycle, re-handshake
clearing the flag, and `record_https_peer_info`'s empty-chain contract.

`rustfmt --edition 2021 --check` was run **in place** and against
`git show HEAD:` of each file: `http_url_connection.rs` 17 deviating hunks
before and 17 after, `ssl_security.rs` 45 and 45. None introduced; the only
textual difference inside a deviating hunk is the three comment lines and the
`.filter(..)` this change added inside `getCipherSuite`'s closure, which rustfmt
already wanted to reindent before this lane touched it. Zero CR bytes in either
file.

---

## 8. What this lane could not settle

* **Any "after" measurement.** No build. Every after is PREDICTED — including
  §1's, which additionally needs `aed6a3b73` in the binary before it can be
  true at all.
* **The four `drain.conn.*` rows.** §3 — the hook is in `native-io`, and the
  honest local alternative (recycling at `getInputStream()`) trades four green
  rows for a divergence in the window HotSpot keeps answering.
* **The four `client.peerHost`/`peerPort` rows.** §4 — the fix is in
  `t27_tls`, and the widening this lane was pointed at would have cost more
  rows than it bought.
* **The four `server.*` rows.** Not investigated, by anyone, yet.
* **Whether `disconnect()` recycling regresses a corpus app.** It cannot regress
  one *relative to HotSpot* — the post-`disconnect()` refusal is measured — but
  an app that only ever worked here, reading `getCipherSuite()` after
  `disconnect()`, now gets an `IllegalStateException` where it used to get a
  suite name. No suite vector calls `HttpURLConnection.disconnect()` at all
  (`grep 'disconnect()' regression-suite/src/*.java` finds one row, and it is a
  `DatagramSocket`). Unmeasurable beyond that without a build.
