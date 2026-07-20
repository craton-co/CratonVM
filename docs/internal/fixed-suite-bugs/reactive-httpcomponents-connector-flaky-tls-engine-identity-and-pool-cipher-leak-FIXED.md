# `reactive.HttpComponentsClientHttpConnectorBuilderTests` — flaky TLS failures under Apache async IOReactor: engine-identity loss + connection-pool cipher-suite leak

**Status: RESOLVED (2026-07-20).** Mechanisms 1 and 2 below are both fixed
by the same change — see "Resolution" at the end of this doc. 45 repeated
runs post-fix: 42 PASS, 3 FAIL, **zero** recurrences of either mechanism's
signature (no crossed ClientHello, no `NoCipherSuitesInCommon`, no generic
`Connection closed by peer`). The 3 residual failures are a **third,
different, newly-found** issue — Tomcat's own embedded-server keystore
loading intermittently throwing `Private key must be accompanied by
certificate chain` — tracked separately, not investigated to root cause in
this pass; see
`docs/known-issues/springboot/tomcat-embedded-server-keystore-empty-cert-chain-intermittent.md`.

**Original write-up follows, found 2026-07-19** while verifying the fix for
[[springboot-tls-sslbundle-trust-validation-gap-cluster]] (now `docs/internal/fixed-suite-bugs/`).
That doc's mechanism #4 (mismatch not rejected) is genuinely fixed —
`connectWithSslBundleAndOptionsMismatch` passes reliably. This is a
**separate, newly-discovered residual**: `reactive.HttpComponentsClientHttpConnectorBuilderTests`
(and likely its sibling `HttpComponentsClientHttpRequestFactoryBuilderTests`,
not yet isolated the same way) is genuinely flaky — repeated runs of the
identical binary against the identical test show a roughly 30-60% failure
rate, with **at least two distinct, independently-confirmed mechanisms**.
Neither mechanism is caused by the SSLBundle trust-validation fix above (that
fix touches `SSLSocket`/`HttpURLConnection`/one read-only `SSLEngineImpl`
getter; neither mechanism below involves any of those).

## Symptom

`connectWithSslBundle(String)` (both GET and POST, the plain success-path
test — the connection is *supposed* to succeed) intermittently fails with
one of several different errors across repeated runs of the same binary:

- `WebClientRequestException: Connection closed by peer`
- `WebClientRequestException: rustls: received unexpected handshake message: got ClientHello when expecting ServerHello or Hello[Retry]Request`
- `WebClientRequestException: Peer certificate chain is empty` (wrapping
  `SSLPeerUnverifiedException`, thrown from
  `AbstractClientTlsStrategy.verifySession`)

Reproduction: run the single class repeatedly (`-ClassList` with just this
class, `-Parallel 1`) via `run-spring-boot-suite.ps1`. 5/5 repeats of the
*isolated* `HttpComponentsClientHttpRequestFactoryBuilderTests` (the sync
variant) pass cleanly — this cluster is specific to the **reactive/async**
connector (`org.apache.hc.core5.reactor` IOReactor + `SSLIOSession`, backed
by `javax.net.ssl.SSLEngine`), not the sync `SSLSocketFactory` path.

Two built-in, permanent diagnostic hooks in this codebase were essential to
diagnosing this (both zero-cost when unset, same precedent as
`CRATONVM_DBG_SC_CLOSE`):

- `CRATONVM_DBG_SC_READ=1` — fingerprints every `SocketChannel.read()` with
  connection id, local/peer address, byte count, and an FNV-1a hash +
  hex-dump prefix (`native-io/src/socket_channel.rs`, originally added for a
  *different* investigation — a Spring STOMP frame double-dispatch bug, see
  the `[SC_READ_REPEAT_STACK]` comment in that file).
- `CRATONVM_DBG_TLS_HS=1` — logs every `SSLEngineImpl.wrap`/`unwrap` call
  with engine id and result (`native-builtins/src/t27_tls.rs`).
- `CRATONVM_DBG_TLS_CIPHERS=1` — **added in this investigation** — logs
  every `SSLEngineImpl.setSSLParameters` call's captured `cipherSuites` list
  per engine id (`native-builtins/src/t27_tls.rs`, `register_apply_parameters`).

## Mechanism 1: engine-identity loss on GC move (crossed ClientHello)

Confirmed via `CRATONVM_DBG_SC_READ` on a live-reproduced failure. Full
timeline (connection ids are `tcp_next_id()`-allocated, monotonic, never
reused — ruled out id-reuse-across-connections as an explanation):

```
t=138824  accept peer=50333 (connection #1)
t=138828  id=0x48 (srv, local=50332 peer=50333) reads ClientHello #1 (254B)
t=138831  id=0x47 (cli, local=50333 peer=50332) reads ServerHello+cert (1699B)
t=138867  accept peer=50334 (connection #2)
t=138870  id=0x4a (srv, local=50332 peer=50334) reads ClientHello #2 (254B) -- correct
t=138876  id=0x49 (cli, local=50334 peer=50332) reads ServerHello+cert (1699B) -- correct
t=139184  id=0x4a (SAME id, SAME local/peer as above) reads ANOTHER, DIFFERENT
          ClientHello (236B, different random) -- ANOMALY. In a passing run,
          this exact (id, local, peer) triple's second read is
          ChangeCipherSpec+encrypted Finished (`14 03 03...17 03 03...`), not
          a second ClientHello.
t=139207  accept peer=50335 (connection #3) -- AFTER the anomalous read, so
          temporally cannot be its source.
```

Ruled out, with direct evidence, in order:

1. **`engine_table` id collision** (two different Java `SSLEngine` objects
   mapping to the same numeric id) — instrumented `engine_id_or_alloc`
   (`native-builtins/src/t27_tls.rs`) directly; a live-reproduced failure
   showed 14 distinct engines, each with a distinct pointer and distinct id,
   zero collisions.
2. **`tcp_registry` id reuse** — `tcp_next_id()` is a plain monotonic
   `AtomicI32` (native-io/src/socket_channel.rs:183-188); each id is issued
   exactly once per process lifetime. The `[SC_ACCEPT]` diagnostic
   (`native-io/src/socket_channel.rs:2602-2615`, itself a prior-investigation
   holdover) confirmed every accept in the failing run was `source=fresh`
   (never through the `nio_selector::take_any_pending_accepted` side-channel
   some AIO paths use), and `take_any_pending_accepted` correctly filters by
   `listener_fd` (`native-io/src/nio_selector.rs:1792-1807`) — ruling out a
   cross-listener accept mixup too.
3. **Registry entry overwrite** — the only two `tcp_registry().write().insert`
   call sites are `tcp_register` (always a fresh id) and
   `tcp_replace_connect_state` (client-side non-blocking-connect state
   transition only — id `0x4a` is a *server*-accepted socket, never goes
   through that path). Neither can explain a mid-connection overwrite.
4. **`try_read_nb`** does a direct `stream.read(buf)` on a specific
   `&TcpStream` (native-io/src/socket_channel.rs:1718-1733) — the OS
   guarantees this can only return bytes from that fd's own kernel receive
   buffer, never another fd's.

With the registry/socket layer clean, the data itself must genuinely be what
the peer sent — meaning **our own client-side TLS engine legitimately sent a
second, fresh ClientHello** on an already-established connection.
`engine_begin` (`native-builtins/src/t27_tls.rs:5539`) *is* idempotent — it
short-circuits if `state.conn.is_some()` — but that guard only protects the
*same* `EngineState`. `engine_table`'s lookup key
(`engine_objref_key`/`objref_key`, `native-builtins/src/t27_tls.rs:3151`,
`4896`) is derived from the Java `SSLEngine` object's **raw pointer**
(`ObjectRef` is a `Hash`-by-pointer-value wrapper around a real heap address,
see `types/src/value.rs:57-70`). CratonVM's GC moves/copies young objects.
**Hypothesis (not yet directly captured with a live GC-move trace, but
consistent with every piece of evidence above): if the GC relocates the
Java `SSLEngine` object between the two `wrap()`/`unwrap()` calls that
should belong to the same handshake, `engine_id_or_alloc` computes a
different hash for the moved object, finds no existing entry, and allocates
a brand-new, empty `EngineState` — orphaning the in-progress handshake and
legitimately starting a second one on the same TCP socket.** This is the
same architectural pattern (pointer-based identity with no GC-move
remapping) as other known issues in this codebase's GC/moving-young-gen
history (see `reference_stale_ref_decode_hardening`,
`reference_descriptor_coercion_slot_reuse_trap` in project memory) — this
would be a new instance of that family, specific to `engine_table`.

**To confirm**: add move-correlated logging (e.g., a GC move hook that logs
old/new `ObjectRef` for any object whose class is `sun/security/ssl/SSLEngineImpl`,
cross-referenced against `engine_id_or_alloc`'s FRESH ALLOC events) and
reproduce again. **To fix** (once confirmed): stop keying `engine_table` by
raw pointer; either store the engine's own allocated id directly in one of
the synthetic object's own field slots (same pattern already used for
`NEW13_SOCK_PORT` etc. on the SSLSocket synthetic), or hook into whatever
GC-move-remapping mechanism this VM uses elsewhere (see
`NativeContext::{add,resolve,remove}_global_root`, used by the
`native-io/src/async_socket.rs` handler-form read completion-delivery fix
for exactly this class of hazard — cross-thread/cross-GC-move stable
identity for a Java object).

## Mechanism 2: connection-pool cipher-suite leak across unrelated test methods

Confirmed via `CRATONVM_DBG_TLS_CIPHERS` on a live-reproduced
`Peer certificate chain is empty` / `NoCipherSuitesInCommon` failure. The
very first engines created in the whole test-class run showed:

```
id=1: cipherSuites=["TLS_AES_256_GCM_SHA384"]                    (single cipher)
id=2: cipherSuites=["TLS_AES_128_GCM_SHA256"]                     (single, DIFFERENT cipher — zero overlap)
id=3: cipherSuites=["TLS_AES_256_GCM_SHA384"]
id=4: cipherSuites=["TLS_AES_128_GCM_SHA256"]
```

versus every other engine in the same run (ids 5, 6, 7, 8, 11-14) getting the
full 9-cipher default list (`native-builtins/src/t27_tls.rs`'s
`DEFAULT_CIPHERS`, echoed back by `getSSLParameters()` when the engine has no
real application-set restriction).

A single, deliberately-mismatched cipher per side is exactly the shape of
`connectWithSslBundleAndOptionsMismatch`'s test design (client offers one
specific cipher, server accepts a different one, asserting the handshake
correctly fails) — **not** `connectWithSslBundle`'s intended configuration
(default, unrestricted). But the actual JUnit failure both times this was
caught was attributed to `connectWithSslBundle[1]` (GET), the
success-expected test, using engine ids 1/2 — the very first ones in the
process.

This points at a **connection-pool reuse bug**: Apache's
`HttpAsyncClient`/HttpCore5 `IOReactor` connection pool appears to hand a
connection (and its already-`setSSLParameters()`-configured `SSLEngine`)
that was built for one test method's specific `SslBundle`/cipher
configuration to a *different* test method expecting a fresh, differently
(or un-)configured connection. **Not yet determined whether this is:**

- a genuine Apache HttpCore5 / Spring Boot test-harness behavior (e.g. a
  shared/cached `ClientHttpConnector` or connection pool across
  `@ParameterizedTest` invocations within the same test class instance) that
  would reproduce on real HotSpot too, or
- a CratonVM-specific bug in connection-open/closed-state tracking that
  makes a connection look poolable/reusable to Apache's client when it
  should have been evicted/treated as belonging to a different pool key.

Distinguishing these needs auditing HttpCore5's own connection-pool routing
logic (`PoolingAsyncClientConnectionManager` and friends) against whatever
CratonVM native state it consults (socket open/closed flags, `SO_*` state) —
a materially different, larger investigation than this doc's scope covers.
**Not investigated further in this pass.**

## Affected classes

| Module | Class | Note |
|---|---|---|
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.reactive.HttpComponentsClientHttpConnectorBuilderTests` | `connectWithSslBundle` flaky (~30-60% fail rate on repetition); `connectWithSslBundleAndOptionsMismatch` passes reliably (that mechanism is fixed, see the retired trust-validation-gap-cluster doc) |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.HttpComponentsClientHttpRequestFactoryBuilderTests` | Sync variant — 5/5 repeats passed clean when isolated; not confirmed affected, but shares the same engine/cipher-propagation code paths, worth re-checking if this doc's mechanisms get fixed and this class starts showing similar flakiness |

## Host-load caveat

This investigation also hit a **shared-host multitenancy confound**: at one
point during repro, 36 concurrent `rustc.exe` + 12 `cargo.exe` processes
(other sessions' builds) were running on the same box, producing 5
consecutive `HANG` results that cleared up once load dropped. `Connection
closed by peer` and `HANG` are plausibly explainable by host-load-induced
timeouts/resets and should be treated with more skepticism than the
`unexpected ClientHello` and `NoCipherSuitesInCommon` symptoms — TCP does
not garble byte content under scheduling pressure, so those two specifically
cannot be explained by host load and are the reliable signal for mechanisms
1 and 2 above.

## Resolution (2026-07-20)

Confirmed a single root cause behind both mechanism 1 (crossed ClientHello)
and mechanism 2 (connection-pool cipher-suite leak): `engine_table` and
`sslparams_alpn_table` (`native-builtins/src/t27_tls.rs`) both keyed Java
`SSLEngine`/`SSLParameters` object identity via `engine_objref_key`, which
hashed the `ObjectRef`'s Debug-formatted **raw pointer value**. `ObjectRef`
is a bare heap pointer (`Hash` implemented on the pointer value itself, see
`types/src/value.rs`), and this VM's young-gen GC moves/reclaims objects —
so a live object's identity was not actually stable across its own
lifetime, and neither table ever evicted stale entries. Once an object
moved (or its old address was reclaimed and reused by an unrelated new
allocation), the new object landing at that address would silently inherit
whatever stale `EngineState`/ALPN list/cipher restriction the table still
had keyed there — explaining both the crossed-handshake symptom (an
orphaned in-progress `EngineState` restarting a fresh ClientHello) and the
cipher-leak symptom (a fresh engine inheriting a stale, narrower cipher
restriction) as two faces of the same bug.

Fix: replaced the raw-pointer hash with `ctx.identity_hash_code(obj)` — the
VM's real, GC-stable identity hash (same contract as `Object.hashCode()`'s
default implementation; already the established pattern elsewhere in this
codebase, e.g. `native-io/src/nio_selector.rs`'s own cross-call Java-object
identity lookups, and the `HttpsURLConnection` instance-keyed config table
from the sibling `tls-sslbundle-trust-validation-gap-cluster` fix). This
required threading a `ctx: &dyn NativeContext` parameter through
`engine_objref_key`, `engine_id_or_alloc` (28 call sites across
`register_engine_impl_natives`, `do_wrap`/`do_unwrap`,
`register_apply_parameters`), and the three `pub(crate)` wrapper functions
(`set_engine_identity_override`, `set_engine_trust_roots_override`,
`set_engine_trust_ctx_key`) plus their one caller in `net_phase_e.rs`'s
`createSSLEngine`. The dead, never-called `engine_id_for` helper was
removed rather than updated.

Verified: 45 repeated runs of the isolated
`reactive.HttpComponentsClientHttpConnectorBuilderTests`
(`-Parallel 1`, one class per process) — 42 PASS, 3 FAIL, zero occurrences
of either original mechanism's signature. The 3 residual failures are a
different, third mechanism (Tomcat's own keystore loading, not this bug —
see the new doc referenced above). Also re-verified all 6 classes from the
sibling `tls-sslbundle-trust-validation-gap-cluster` fix — all still PASS,
no regression.

**Scope note**: `objref_key` (a sibling helper with the identical
raw-pointer-hash flaw, used by unrelated side tables — `sock_alpn_table`,
`session_peer_certs_table`, and others) was intentionally **not** touched
in this pass; fixing it would be a much larger, separate refactor across
many more call sites, and none of those tables were implicated in the
mechanisms fixed here. Worth revisiting if a similar identity-loss symptom
turns up elsewhere.
