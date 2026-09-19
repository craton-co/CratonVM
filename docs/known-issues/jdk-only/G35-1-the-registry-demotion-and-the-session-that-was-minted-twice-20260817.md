# G35-1 — the registry demotion, and the session that was minted twice

**Status:** PARTIAL. **Before: MEASURED** on `C:/craton/target-rel3/release/cratonvm.exe`
(`9ae371468`) and cross-checked on `target-rel2` (`9964ca733`). **After: NOT
MEASURED — and could not be.** This lane's brief forbids `cargo build`,
`cargo check` and `cargo test`, so no binary exists containing the code below.
Every "after" is **PREDICTED** and labelled so at every occurrence.

What *is* measured: the oracle (Temurin 25.0.3+9-LTS), the before-state of both
vectors on the newest binary, the before-state of all 121 rows in the tables,
the native-registry ownership dump, and — for §1 — the new parser's output on
every one of those rows, obtained by extracting the transcribed functions into a
standalone `rustc --test` harness outside the crate tree
(`scratchpad/rs/{core,main,auth,tests}.rs`; `rustc`, never `cargo`, and no
artifact written into `target/`). 6 of the 6 new URI tests pass there. That is
not the same as running them in the crate, and it is not claimed to be.

**Owned file:** `native-builtins/src/net_phase_e.rs`. Everything else is a
NOMINATION in §6. Probes: `scratchpad/probe/{PA,PB,PC}.java`.

---

> **VERIFIED AGAINST A BINARY 2026-09-03.** "After: NOT MEASURED — and could not
> be." Two of §0's three predictions have now been measured, and both land at or
> beyond what they promised.
>
> ```text
>                        predicted            measured (--jdk-only, vs HotSpot 25)
> RJdkBridge1            PASS, 394 checks     PASS, 483 checks   diff EMPTY
> RSslLiveSession        13 failing rows      fails=0            diff EMPTY
> ```
>
> **`RJdkBridge1` — the prediction was exactly right.** §0 measured it dying in
> `uri` at `RJdkBridge1.java:1181` on `negp.getPort() == -1`, with the five
> completed sections summing to 154 and `sectionEnd("uri", 50)` never reached. It
> now runs to `PASS RJdkBridge1 (483 checks)`, byte-identical to the oracle. 483
> rather than 394 because the vector grew; the count is the vector's OWN
> self-report on its last line, not a line count — the output is 182 lines, and
> reading those as checks would have looked like a run that still stops early.
>
> **`RSslLiveSession` beat its prediction.** §0 measured 95 checks with **14
> failing rows** and predicted **13**. The vector's own tally now reads
> `CK RSslLiveSession fails=0`, on both VMs, with an empty diff. Not 13, not 1 —
> none.
>
> **This record and `G14-1` had the SAME before-state and predicted opposite
> outcomes, and both were right about their own reach.** Both measured
> `RJdkBridge1` dying on `negp.getPort() == -1`. `G14-1` predicted "still dies at
> check 197" because the blocking fix lived in `net_phase_e.rs`, which that lane
> could not edit. This lane owned that file and predicted PASS. The vector
> passes. A prediction that names what it cannot reach is worth more than one
> that hedges.
>
> **Still unmeasured:** §0's third row, the URI authority surface at 51 of 121
> diverging rows predicted to 0. That needs its own row-by-row run and this note
> did not do it.

## 0. The headline

| vector | before (MEASURED, `9ae371468`) | after (PREDICTED) |
|---|---|---|
| `RJdkBridge1` | dies in `uri` at `RJdkBridge1.java:1181` (`negp.getPort() == -1`); the five completed sections print `props=40 treenav=42 collect=19 deque=33 vector=20` = **154**, and the `uri` section never reaches its own `sectionEnd("uri", 50)` | **PASS, 394 checks** |
| `RSslLiveSession` | runs to the end: **95 checks, 14 failing rows** | **13** failing rows; 12 with the §6 N1 call site, 8 with N2 as well |
| URI authority surface | **51 of 121 measured rows diverge** | 0 |

`RSslLiveSession` no longer aborts — `2944095fe` fixed the verifier failure that
stopped it at 67 rows on `9964ca733`, and this lane re-measured that rather than
assuming it.

---

## 1. `java.net.URI`'s authority is server-based or it is nothing

The failing assertion is `RJdkBridge1.java:1181`:

```java
URI negp = new URI("http://h:-5/p");
check(negp.getPort() == -1, "':-5' is not a port ...");
```

`net_phase_e::uri_parse_authority` ended in `p.parse::<i32>().ok()`, and Rust's
integer parser takes a leading `+` or `-`. Java's port production is `*DIGIT`.

**But the rule is much wider than the sign, and clamping the port would have
passed line 1181 and failed line 1183.** Reading `Parser.parseAuthority` in
`$JAVA_HOME/lib/src.zip!java.base/java/net/URI.java` settles it:

```java
} catch (URISyntaxException x) {
    // Undo results of failed parse
    userInfo = null;
    host = null;
    port = -1;
```

When the *server-based* parse fails and `requireServerAuthority` is false —
which it is for **every** `new URI(String)` — the JDK does not throw. It undoes
the attempt and keeps the authority as *registry-based*. `userInfo`, `host` and
`port` are discarded **together**. That is why `new URI("http://h:-5/p")` is a
perfectly legal URI whose `getAuthority()` is `h:-5` and whose `getHost()` is
`null`.

### 1a. The port

MEASURED, both VMs, 2026-08-17. CratonVM's column is identical on `9964ca733`
and `9ae371468`.

| input | HotSpot `ui/host/port` | CratonVM |
|---|---|---|
| `http://h:-5/p` | `null` / `null` / `-1` | `null` / **`h`** / **`-5`** |
| `http://h:+80/p` | `null` / `null` / `-1` | `null` / **`h`** / **`80`** |
| `http://h:-0/p` | `null` / `null` / `-1` | `null` / **`h`** / **`0`** |
| `http://h:8x/p` | `null` / `null` / `-1` | `null` / **`h`** / `-1` |
| `http://h:x80/p` | `null` / `null` / `-1` | `null` / **`h`** / `-1` |
| `http://h:80x80/p` | `null` / `null` / `-1` | `null` / **`h`** / `-1` |
| `http://h:99999999999/p` | `null` / `null` / `-1` | `null` / **`h`** / `-1` |
| `http://h:2147483648/p` | `null` / `null` / `-1` | `null` / **`h`** / `-1` |
| `http://h:4294967296/p` | `null` / `null` / `-1` | `null` / **`h`** / `-1` |
| `http://h:80:90/p` | `null` / `null` / `-1` | `null` / **`h:80`** / **`90`** |
| `http://h::80/p` | `null` / `null` / `-1` | `null` / **`h:`** / **`80`** |
| `http://u@h:x/p` | `null` / `null` / `-1` | **`u`** / **`h`** / `-1` |
| `http://u:pw@h:8x/p` | `null` / `null` / `-1` | **`u:pw`** / **`h`** / `-1` |
| `http://u@1.2.3.4:8x/p` | `null` / `null` / `-1` | **`u`** / **`1.2.3.4`** / `-1` |

The last two rows in that table are the ones a port-only fix cannot reach, and
`h:80:90` is the one a "split on the last colon" fix cannot: the JDK's port span
is `scan(p, n, "/")`, and an authority contains no `/`, so **the port text runs
to the end of the authority**. Two colons therefore mean a non-digit in the
port, not a second field.

The boundary rows that keep the refusal from over-firing — all already green,
all kept green:

| input | HotSpot port |
|---|---|
| `http://h:007/p` | `7` |
| `http://h:080/p` | `80` |
| `http://h:00000000080/p` | `80` |
| `http://h:0000000000000/p` | `0` |
| `http://h:2147483647/p` | `2147483647` |
| `http://h:65536/p` | `65536` (there is no range check) |
| `http://h:/p` | `-1`, host `h` |

Leading zeros are not significant digits and cannot overflow; only
`Integer.parseInt` failing is `Malformed port number`, and that demotes too.

### 1b. The empty host

| input | HotSpot | CratonVM |
|---|---|---|
| `http://:80/p` | `null` / `null` / `-1` | `null` / `null` / **`80`** |
| `http://u@:80/p` | `null` / `null` / `-1` | **`u`** / `null` / **`80`** |

Nothing is wrong with `:80` as a port. What fails is `parseHostname`'s
`if (l < 0) failExpecting("hostname", start)` — no label was parsed at all — and
the demotion then takes the perfectly good port with it. Three rows would have
suggested "bad port ⇒ no port". The family says "bad *anything* ⇒ no server
authority".

### 1c. The hostname grammar, which is not "whatever is not a port"

This is the part the previous lane deferred as too large to do from three rows
(G14-1 §6). With the full oracle in hand it is one function,
`Parser.parseHostname`, and it is transcribed rather than derived. MEASURED —
HotSpot answers a **null host** for every row here, CratonVM answered the
literal text:

| input | why HotSpot refuses |
|---|---|
| `http://a_b/p` | `_` is legal in a reg-name, not in a domain label |
| `http://a..b/p`, `http://a../p`, `http://.a/p` | empty label |
| `http://-h/p` | a label must start alphanumeric |
| `http://h-/p` | and must not end with `-` |
| `http://a.9b/p` | **multi-label: the last label must start with a LETTER** |
| `http://1.2.3/p`, `http://1.2.3.4.5/p`, `http://1.2.3.4x/p` | not an IPv4 address, and the last label is digit-leading |
| `http://256.1.1.1/p`, `http://192.196.0.5555/p` | octet out of range, then the same |
| `http://h$x/p` `h,x` `h;x` `h=x` `h&x` `h!x` `h~x` `h*x` `h'x` `h(x)` | legal in a reg-name, not in a host |
| `http://h%20x/p`, `http://h%41x/p` | an escape is legal in a reg-name, not in a host |
| `http://a@b@c/p`, `http://u@h@/p`, `http://u@h:80@x/p` | the FIRST `@` delimits, so the remainder must parse as a host |

…and the rows that must stay accepted, which is where the "last label starts
with a letter" rule shows its exact shape (`l > start`, so a **single** label is
exempt):

| input | HotSpot host |
|---|---|
| `http://9h/p`, `http://12/p` | `9h`, `12` — single label, rule does not apply |
| `http://a.b9/p` | `a.b9` — last label starts with `b` |
| `http://h./p`, `http://a.b./p` | `h.`, `a.b.` — a trailing dot is fine |
| `http://a-b.c-d/p`, `http://xn--d1acufc.xn--p1ai/p` | interior dashes are fine |
| `http://01.2.3.4/p` | `01.2.3.4` — leading zeros are not significant digits |
| `http://1.2.3.4/p`, `http://255.255.255.255/p` | real dotted quads |

`http://例え.jp/p` is `null` on **both** VMs before and after: the JDK's
`L_ALPHANUM` is an ASCII mask, and so is the transcription.

### 1d. What was NOT touched, deliberately

* **`url_parse`.** `java.net.URL`'s port grammar is `Integer.parseInt`, which
  takes the `+`. MEASURED: `new URL("http://h:+80/p").getPort()` is **80** on
  HotSpot while `new URI("http://h:+80/p").getPort()` is `-1`. Tightening the
  shared parser would have broken URL, and all eleven measured URL rows are
  already green.
* **`checkChars(userInfo)`.** Not transcribed, and that is not a gap:
  `L_REG_NAME` and `L_USERINFO` differ by exactly one character, `@`, and an `@`
  inside the user-info span can only arise if a *later* `@` were chosen as the
  delimiter — which never happens, because the JDK stops at the first. Every
  other character legal in a user-info but not in an authority is already
  refused upstream by the whole-authority character check. `http://a@b@c/p`
  measured `null` confirms the reasoning end to end.
* **The IPv6 literal body.** `parseIPv6Reference` and the scope-id rules are
  `net_uri_inet.rs`'s (G14-1 §3–§5), and on `9ae371468` bracketed authorities
  already refuse correctly — `http://[::1]:-5/p` throws `Illegal character in
  port number at index 13` on both VMs. This change only decides where the host
  ends, and leaves the body to the constructor that already polices it.

### 1e. `getPort` no longer reads a slot another grammar wrote

`getPort` began with `if let Value::Int(p) = …"port" { if p > 0 { return p } }`,
and only fell through to the raw string when that was non-positive. That
shortcut is a second model, and it was actively masking: `url_parse` writes that
same slot with URL's looser grammar, so a URL-shaped `80` answered for
`http://h:+80/p` before the URI parser was ever consulted, and for the
authority-less `urn:isbn:0451450523` the slot held **`451450523`**.

`getPort` now derives from the raw string first and falls back to the slot only
when there is no raw string — which is exactly what `getHost` beside it already
did, and for the reason its own comment gives. The two accessors can no longer
disagree about whether the authority demoted.

**Fixed** — §1a–§1e, 51 rows. **After: PREDICTED**, but the transcription's
output was checked against all 121 oracle rows in the standalone harness and
matched every one.

---

## 2. `getSSLSession()` minted a new object on every call

MEASURED, `RSslLiveSession` on `9ae371468`:

```text
CK RSslLiveSession client.sslSession.sameObjectTwice   = false  WANT true
CK RSslLiveSession verifier.sameObjectAsGetSSLSession  = false  WANT true
```

`https_session_object` allocated a fresh `javax/net/ssl/SSLSession`, filled in
four fields and returned it — **per call**. The carrier's side table held the
handshake's *data* (protocol, cipher, peer chain) and no object, so nothing tied
two calls together. This was invisible while `getId()` answered `byte[0]`; it
became visible the moment the ids were real, because `getId()` is seeded from
the object's identity, so two reads of one connection disagreed about one
handshake.

**Fixed** by caching one session per carrier. The cache is a **global-root
handle** (`NativeContext::add_global_root` → `usize`), stored in the existing
`HttpsCarrierSession` struct, so the "this table holds no `ObjectRef`" rule the
table's own comment states is preserved *exactly*: a handle is plain data, the
collector owns the reference, and `resolve_global_root` hands back the current
address after a move. Same shape as `jca::provider_chain` and
`jboss_jdkspecific`, which already cache a Java object this way.

Three things had to come with it, and each is a defect in its own right if
omitted:

1. **The lookup precedes the allocation.** An allocation-first body that only
   *then* consulted the table would still mint one session per accessor call and
   cache only the last. Asserted against the source, since no behavioural test
   can see it without a live TLS peer.
2. **The carrier is pinned across the allocations.** The publish step re-reads
   the carrier to key the table, and a raw `ObjectRef` does not survive a moving
   young GC — `identity_hash_code` on a vacated from-space address is not the
   identity of anything.
3. **A second handshake on the same carrier releases the old root**, rather than
   pinning that `SSLSession` for the life of the VM. This is why
   `record_https_carrier_session` now takes `&mut dyn NativeContext`; its single
   call site already passes one, so the signature change is invisible to it.

**Fixed:** `client.sslSession.sameObjectTwice`. **After: PREDICTED** — 1 row.

### 2a. The verifier's session is a different minter, in a different file

`verifier.sameObjectAsGetSSLSession` cannot be closed from here alone.
`http_url_connection.rs::huc_verify_hostname` mints its **own** session (the
same four `set_field` calls, deliberately sharing `HTTPS_CLIENT_SESSION_MARKER`
so the two cannot drift) and hands that to the application's `HostnameVerifier`.
Two minters cannot produce one object however identical their writes are.

This file now exposes `https_carrier_session_object(ctx, connection)` — the one
minter, behind the one cache — for that call site to use instead. **N1 in §6.**
Until it lands the row stays red; nothing regresses.

### 2b. The drain trap, and the table's only eviction path

MEASURED on HotSpot: once the response body is drained the connection returns to
the `KeepAliveCache` and every CONNECTION-level accessor throws
`IllegalStateException: connection not yet open` again, while the SESSION object
the application holds stays valid. CratonVM keeps answering — four rows:

```text
drain.conn.cipherSuite.raises  = none  WANT java.lang.IllegalStateException
drain.conn.cipherSuite.message = none  WANT connection not yet open
drain.conn.sslSession.raises   = none  WANT java.lang.IllegalStateException
drain.conn.sslSession.message  = none  WANT connection not yet open
```

The place that knows a body reached EOF is the `https:` input stream in
`http_url_connection.rs`, so this file can only supply the entry point:
`forget_https_carrier_session(ctx, connection)`, added here. **N2 in §6.**

It matters for a second reason. `https_carrier_sessions` has never evicted
anything — one entry per HTTPS carrier ever handshaked, for the life of the VM —
and as of this commit each entry also holds a global root on an `SSLSession`.
`forget_https_carrier_session` is that table's first and only eviction path.
While it stays uncalled the growth is the pre-existing one plus one live session
object per carrier; that is stated rather than hidden.

---

## 3. The registrar collapse, re-measured rather than inherited

The brief warned that another lane had since edited `http_url_connection.rs`.
`--dump-native-registry` under `--jdk-only` on `9964ca733`, both carrier
classes, identical rows — **the collapse is unchanged, only the line numbers
moved**:

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

`getSSLSession` is the only one of the six this file owns, and it is the only
one either §2 assertion goes through. `invocations` is `0` on every row of that
dump and proves nothing — the probe it was taken from makes no HTTPS request.
`owns_slot` is the field that settles it.

---

## 4. What this lane could not settle

* **Any "after" measurement.** No build. Every after is PREDICTED.
* **`RJdkBridge1`'s remaining `uri` rows and everything after them.** The `uri`
  section is 50 checks; the rows still hidden behind the first failure are
  `getPort`/`getHost`/`getAuthority` on the demoting inputs plus `URI.toURL`,
  and §1 addresses all of them — but nothing downstream of the failing
  assertion has ever executed on this VM, including the whole `bytebuf` and
  `bigint` remainder that HotSpot reaches. The PASS in §0 is a prediction on
  that basis and nothing stronger.
* **`new URI("http://")`.** HotSpot throws `Expected authority at index 7`;
  CratonVM builds a URI with a null authority. That is the constructor's
  refusal, not the accessor's — one row, not chased.
* **`client.peerHost` / `peerPort`** (4 failing rows). The 4-field client
  session shape has no peer-host slot (`NEW13_SSL_SESS_FIELDS = 4`: proto,
  cipher, tls-id, attributes), so this needs the shape widened in
  `phases_late/ssl_security.rs` **and** the accessors in `t27_tls.rs` taught to
  read it. Two files, neither mine. **N3.**
* **The four `server.*` rows** (`localPrincipal`, `localPrincipal.class`,
  `localCertificates.length`, `peerPort.isPositive`) are server-side sessions
  and were not investigated at all.
* **Whether the wider host grammar regresses any corpus app.** It cannot
  regress one *relative to HotSpot* — every row above moves toward the oracle —
  but `getHost()` now answers `null` for reg-name authorities such as
  `http://a_b/` that this VM used to accept, and `uri_components` (which
  `http2.rs` and `servlet.rs` use to pick a connect target) follows it. HotSpot
  behaves identically, so an app that works there works here; an app that only
  ever worked *here* may not. Unmeasurable without a build.

---

## 5. What changed, and how it is guarded

`native-builtins/src/net_phase_e.rs` only:

| change | rows |
|---|---|
| `uri_parse_server_authority` / `uri_parse_hostname` / `uri_parse_ipv4_address` / `uri_scan_ipv4_address` / `uri_scan_byte` — transcribed from `URI.java`; `uri_parse_authority` becomes the demoting wrapper | §1a–§1c, 51 |
| `getPort` derives from the raw string first, slot only as fallback | §1e (subsumed above) |
| `HttpsCarrierSession.session_root` + cache-first `https_session_object` + `https_cached_session_object` | §2, 1 |
| `record_https_carrier_session` takes `&mut` and releases the superseded root | §2, leak |
| `https_carrier_session_object` — the exposure N1 needs | §2a, 0 until N1 |
| `forget_https_carrier_session` — the entry point N2 needs, and the table's first eviction path | §2b, 0 until N2 |
| the registrar's `MEASURED` stanza re-taken on `9964ca733` | §3 |

**Nine new tests** in the existing `#[cfg(test)] mod tests`:

* six over `uri_parse_authority`, one per family, asserting every oracle row in
  §1a–§1c *and* the accept-lists that keep the new refusals from over-firing;
* `re_recording_a_carrier_handshake_drops_the_cached_session_handle` — the
  invalidation, which is the half that leaks if it is missing;
* `forgetting_a_recycled_carrier_removes_the_entry_entirely`, including that a
  second call is a no-op (a body drained twice must not release a handle twice);
* `the_session_cache_lookup_precedes_the_allocation` — a source witness, because
  the ordering in §2.1 has no behavioural test without a live TLS peer.

The six URI tests were **run** in the standalone `rustc --test` harness
described in the header and all pass; the three session tests were not, because
they need `MockNativeContext`. None has been run inside the crate.

`rustfmt --edition 2021 --check` was run **in place**: 452 deviating hunks
before this change and 452 after, none introduced — the only textual difference
inside a deviating hunk is one line whose arguments this change edited. Zero CR
bytes.

---

## 6. NOMINATIONS (outside this lane's file)

**N1 — `http_url_connection.rs::huc_verify_hostname`: hand the verifier THE
session, not a second one. Closes `verifier.sameObjectAsGetSSLSession`.**
The block that allocates `javax/net/ssl/SSLSession` and writes
`NEW13_SESS_PROTO` / `NEW13_SESS_CIPHER` / `NEW13_SESS_TLSID` and then calls
`record_client_peer_chain` (STEP 2, immediately before the `invoke_virtual` of
`verify`) should be replaced by

```rust
let session = match crate::net_phase_e::https_carrier_session_object(ctx, conn) {
    Some(Ok(s)) => s,
    // no recorded handshake, or the allocation was refused — keep the
    // existing local mint as the fallback
    _ => { /* existing block */ }
};
```

`record_https_carrier_session` runs at STEP 0, above the built-in check's early
return, so by STEP 2 the entry always exists. This is safe to do *before* it:
the exposed function returns `None` rather than failing when there is no entry.
MEASURED HotSpot contract: the object the verifier is given **is** the object
`getSSLSession()` returns afterwards.

**N2 — `http_url_connection.rs`: call `forget_https_carrier_session` when an
`https:` response body reaches EOF. Closes the four `drain.*` rows.**
HotSpot returns the connection to the `KeepAliveCache` at that point and every
connection-level accessor throws `IllegalStateException: connection not yet
open` again; the session object the application already holds stays valid, which
is the row that separates "recycled" from "destroyed" (`drain.session.isValid`
is `true` and already green). It is also the only way anything is ever removed
from `https_carrier_sessions`.

**N3 — `phases_late/ssl_security.rs` + `t27_tls.rs`: the client `SSLSession`
shape has nowhere to put the peer host or port. 4 rows.**
MEASURED on `9ae371468`: `client.peerHost = null WANT localhost`,
`client.peerPort.isServerPort = false`, and the same pair again under
`attrs.shadow.*`. `NEW13_SSL_SESS_FIELDS = 4` (proto, cipher, tls-id,
attributes) and `getPeerHost`/`getPeerPort` are registered in `t27_tls.rs:18135`
and `:18163`. Widening the shape touches every minter of it, including two in
`net_phase_e.rs` and `http_url_connection.rs`, so it wants one owner and one
pass — not five files editing a width.

**N4 — `net_uri_inet.rs`: `http://[::1]: /p` reports the wrong index, and
`parseAuthority` says why.**
G14-1 §6 left this unpinned as "one row is not enough". It is derivable now.
HotSpot reports `Illegal character in authority at index 7` — the authority's
start — because `parseAuthority`'s post-mortem is
`fail("Illegal character in authority", serverChars ? q : qreg)`, and with a
`[` present the *reg-name* scan stops at the `[` (index 7) while the server scan
stops at the space (index 13). `serverChars` is false, so `qreg` is used.
MEASURED sibling that confirms the rule: `http://h: /p`, with no bracket, is
`… at index 9` — the space itself, because there `qreg` is the space too.

**N5 — the five dead bodies in this file's `register_https_session_accessors`.**
Unchanged from the note already in the source and re-measured in §3: five of the
six accessors here are shadowed by `http_url_connection.rs`. They are left in
place because they are the registration for any carrier class that file does not
cover, but a reader should not believe a change to them takes effect. Deleting
or de-duplicating them is a decision for whoever owns both files.

**N6 — G14-1's N2…N7 are untouched and still open.** Percent-decoding inside
`[…]`, `URI.create` message relay, `URI.create(null)`, the `URISyntaxException`
fields lost inside JDK frames, `URI.normalize`'s leading `..`, and
`URI.resolve("")`. None is in the authority path this lane rewrote, and none was
re-measured here.
