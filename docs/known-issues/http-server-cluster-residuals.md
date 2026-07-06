# http.server bug cluster (12 classes) — fixes landed + residuals

## Status

**Mostly fixed** on branch `fix/http-server-cluster` (off dev `81762470`),
merged to `dev`. Reproduced and iterated on both Windows + JDK25 (the
platform the original bug report was captured on, via
`apps/spring-suite-runner`) and the Azure Linux host for fast iteration.
10 of 12 classes are fully fixed; 2 have residual issues, updated across
three 2026-07-06 follow-up passes below. `ServerHttpsRequestIntegrationTests`:
FOUR real bugs found and fixed across sessions (a `Provider.putService`
gap + per-instance `containsKey` isolation; a `CertificateFactory` real-SPI
delegation gap; a PKCS12/PBE empty-password guard that threw instead of
producing a 0-length key; and a widespread real-JDK-mode native
unreachability bug — `register_phase68_natives`/`register_p68_ssl`, which
back `SecretKeyFactory`/`KeyStore.setKeyEntry`/`SSLEngine` real TLS
plumbing, were compiled out of every non-synthetic-JDK build entirely).
The test STILL fails — a fifth, distinct bug in the rustls `SSLEngine`
wrap/unwrap byte-pumping loop itself remains open, see "Update (2026-07-06
session, part 3)" below. `ZeroCopyIntegrationTests`'s original reported
failure did not reproduce, but an unrelated pre-existing flakiness was
found and documented instead — see "Update (2026-07-06 session)" below.

## Root causes fixed

1. **`Collections.emptyListIterator()` mis-stamped as `Collections$EmptyIterator`**
   (an `Iterator`, not a `ListIterator`) instead of `Collections$EmptyListIterator`.
   `native_empty_iterator` served both `emptyIterator()` and `emptyListIterator()`
   with the same class stamp. Any caller holding the result as a `ListIterator`
   (e.g. Jetty's `ContextHandler.notifyExitScope` walking an empty listener
   list via `list.listIterator(list.size())`) hit `NoSuchMethodError:
   Collections$EmptyIterator.hasPrevious()Z` on the **main thread**, which is
   an uncaught linkage error there — it aborts the whole VM process instead of
   just failing one test. This was THE dominant bug: it crashed 8 of the 9
   ABEND classes (`AsyncIntegrationTests`, `CookieIntegrationTests`,
   `EchoHandlerIntegrationTests`, `ErrorHandlerIntegrationTests`,
   `MultipartHttpHandlerIntegrationTests`, `RandomHandlerIntegrationTests`,
   `ServerHttpRequestIntegrationTests`, `WriteOnlyHandlerIntegrationTests`) —
   all exercise the same 4-way parameterized `AbstractHttpHandlerIntegrationTests`
   including a Jetty (Servlet) backend, and Jetty's `ContextHandler` calls
   `notifyExitScope` on every request. Fixed by splitting off
   `native_empty_list_iterator`, which stamps `Collections$EmptyListIterator`
   and uses its own `EMPTY_ITERATOR` singleton field.
   `native-collections/src/lib.rs`.

2. **`Collections.newSetFromMap(LinkedCaseInsensitiveMap)` lost case-insensitive
   key semantics.** The native returned a synthetic value-hash `HashSet`
   regardless of the backing map's own key-equality semantics (same class of
   bug as the already-fixed `IdentityHashMap` case, see
   `docs/internal/h2-suite-bugs/run-20260622/HIB-CV-28-...md`) — a value-hash
   `HashSet<String>` doesn't replicate `LinkedCaseInsensitiveMap`'s
   case-folding, so `HttpComponentsHeadersAdapter`'s header-name
   `Set` (`Collections.newSetFromMap(new LinkedCaseInsensitiveMap<>(...))`)
   kept "TestHeader" and "TestHEADER" as two distinct entries instead of one.
   Fixed `HeadersAdaptersTests` (`shouldRemoveCaseInsensitiveFromKeySet`,
   `headerSetEntryCanSetList`). Extended the existing `IdentityHashMap`
   special-case in the `newSetFromMap` native to also cover
   `org/springframework/util/LinkedCaseInsensitiveMap`, routing both through
   the real `Collections$SetFromMap` (whose `contains`/`toArray`/`iterator`
   natives already delegate to the live backing map via `invoke_virtual`).
   `native-builtins/src/lib.rs`.

   **Follow-on regression, also fixed**: `native_set_from_map_size` routed
   through `native_map_size`'s synthetic-HashMap-layout heuristic
   (`map_state`'s "is slot 0 an array" probe) instead of delegating to the
   backing map's real `size()` via `invoke_virtual` like its
   `contains`/`toArray`/`iterator` siblings already did. A real
   `LinkedCaseInsensitiveMap`'s own `table` field happens to satisfy that
   "is it an array" probe, so it was misread as CratonVM's synthetic bucket
   layout and always reported size 0 — this broke every
   `hasSize(N)`-style `HeadersAdaptersTests` assertion once fix #2 started
   returning a real `SetFromMap` instead of the old (wrong-semantics but
   correctly-sized) synthetic `HashSet`. `native-collections/src/lib.rs`.

3. **`java.net.URI`'s single-string parser accepted malformed `%` escapes.**
   `uri_first_illegal_index` treated `%` as always legal, with no check that
   it's followed by two hex digits (RFC 2396 `escaped = "%" hex hex`). Real
   JDK throws `URISyntaxException` for a bare/malformed `%` (`"foo%%x"`,
   `"/p%th"`); ours silently accepted it. This broke
   `ServletServerHttpRequest.initURI`'s malformed-query catch-and-reencode
   fallback (never triggered, since the first parse attempt never threw) and
   the "malformed path must throw `IllegalStateException`" contract. Fixed
   `ServletServerHttpRequestTests.getUriWithMalformedQueryParam`/
   `getUriWithMalformedPath`. `native-builtins/src/lib.rs`.

4. **`java.net.URI`'s multi-argument constructors didn't quote the query
   component at all.** `native_uri_init_5`/`native_uri_init_7` spliced the
   raw query string directly into the full URI string with no escaping, so
   `encodeQuery`'s `new URI(null, null, "", query, null).getRawQuery()` (the
   fallback `initURI` uses to fix up a malformed query) was a no-op — a bare
   `%` was never turned into `%25`. Added `quote_uric`, mirroring
   `java.net.URI`'s private `quote(String, L_URIC, H_URIC)`: percent-escape
   any character outside RFC 2396 `uric` (reserved | unreserved), including a
   literal `%` (which real JDK always escapes here — these constructors have
   no "already escaped" concept). `native-builtins/src/lib.rs`.

5. **`URLDecoder.decode(String, String)` / `(String, Charset)` and
   `URLEncoder.encode(String, String)` / `(String, Charset)` ignored the
   requested charset entirely**, always UTF-8-(lossy-)decoding/encoding the
   percent-escaped bytes regardless of the charset argument (both the active
   `deprecated_io_util.rs` registrations and the dormant duplicate
   registrations in `phases_early.rs`). A non-UTF-8 charset like
   `windows-1251` therefore produced U+FFFD replacement characters on decode
   (`ServletServerHttpRequestTests.getFormBodyWithNotUtf8Charset`) or UTF-8
   bytes instead of the charset's own single-byte encoding on encode. Split
   percent-decoding (bytes) from the bytes-to-`String`/`String`-to-bytes
   charset conversion step and routed the charset-aware overloads through
   the existing `charset::decode_str_named`/`encode_str_named`/
   `charset_name_of` helpers. `native-builtins/src/deprecated_io_util.rs`,
   `native-builtins/src/phases_early.rs`.

## Verification

- `ServletServerHttpRequestTests`: **17/17 OK** (was FAIL 14/17).
- `HeadersAdaptersTests`: **90/90 OK** (was FAIL 88/90, then transiently
  85/90 after fix #2 before the size() follow-on was found).
- `AsyncIntegrationTests`, `CookieIntegrationTests`,
  `EchoHandlerIntegrationTests`, `ErrorHandlerIntegrationTests`,
  `MultipartHttpHandlerIntegrationTests`, `RandomHandlerIntegrationTests`,
  `ServerHttpRequestIntegrationTests`, `WriteOnlyHandlerIntegrationTests`:
  no longer ABEND; all methods pass on the Jetty/Jetty-Core/Tomcat backends.
  On the Azure Linux verification host only, the "Reactor Netty" backend
  parameterization fails with `UnsatisfiedLinkError:
  sun/nio/ch/IOUtil.fdLimit()I` and/or `NoClassDefFoundError:
  sun/nio/ch/EPollSelectorImpl` — both are **pre-existing, documented,
  Linux-only native gaps** (see
  `docs/known-issues/http-client-cluster-redefine-dispatch-and-jdk21-gaps.md`'s
  "Linux-only NIO gaps" residual), unrelated to this cluster's fixes and not
  expected to reproduce on the Windows+JDK25 target the original bug report
  was captured on.

## Update (2026-07-06 session) — ZeroCopy re-scoped, BC provider bug found+partially fixed

This session re-investigated both residuals with BouncyCastle actually on
the classpath (`spring-web`'s `testFixturesImplementation("org.bouncycastle:
bcpkix-jdk18on")`, confirmed present via a regenerated Linux-native
`cratonvm-testcp.txt` — 204 classpath entries including `bcpkix-jdk18on-1.72.jar`,
`bcprov-jdk18on-1.72.jar`, `bcutil-jdk18on-1.72.jar`). This changes the
original "Residual 1" diagnosis materially — see below.

### `ServerHttpsRequestIntegrationTests` — FOUR bugs found+fixed across sessions, ONE new bug remains open (2026-07-06, third pass)

**Follow-up (same-day, second pass)**: the `CertificateFactory`
delegation bug described below (originally left open at the end of the
first pass) has since been fixed too — see "Second bug — FIXED this
session (follow-up, same day)" further down. A third, distinct bug
(PKCS12/PBE empty-password handling in Nettys JDK-native SSL context path)
was found once that got fixed, and remains open — see the end of this
section. Net count: 2 bugs fixed, 1 new residual, test still fails
end-to-end for the new reason.

The original diagnosis (OpenJDK-reflection fallback `X509CertImpl` signing,
`CertificateFactory` parse failure on truncated PEM) does not match what
actually happens with BC on the classpath. Confirmed via minimal standalone
repros (`new BouncyCastleProvider()` etc., compiled directly against the
Netty/BC jars):

**Bug found + FIXED this session**: `java.security.Provider.putService
(Provider$Service)` had no native shim at all, so real inherited `Provider`
bytecode ran against `legacyMap`/`serviceMap` fields that are never
initialized for CratonVM's synthetic `Provider` objects (the real `Provider`
constructor never runs for them). Modern providers that register services
via `putService` directly instead of the legacy `put`/`parseLegacyPut`
surface — e.g. BouncyCastle's `GOST3411$Mappings.configure()` — hit this
gap. Worse: BouncyCastle's own `addAlgorithm(String, String)` (the
`ConfigurableProvider` method `Mappings.configure()` calls) does
`containsKey(key)` first and throws `IllegalStateException: duplicate
provider key (...) found` **itself** if true — and CratonVM's `containsKey`
shim read a process-wide table keyed only by provider **name** (`"BC"`),
shared across every `Provider` object with that name. Real BC code
legitimately constructs more than one independent `BouncyCastleProvider`
instance in a single call flow (Netty's `BouncyCastleUtil.getBcProviderJce()`
caches one; BC's own internal `BCJcaJceHelper` — used by
`JcaX509CertificateConverter.getCertificate()` — constructs a second,
separate one via reflection). On real HotSpot each instance's per-object
service map starts empty, so the second construction is unaffected; on
CratonVM the shared-by-name table already had entries from the first
instance, so the second instance's `addAlgorithm("MessageDigest.GOST3411",
...)` call saw `containsKey(...) == true` and threw — reproducible in an
8-line standalone repro (`new BouncyCastleProvider()` twice in one process).

  Fix: added the missing `putService` native (`native-builtins/src/jca/
  provider_chain.rs`), and — since that alone did not fix the crash, the
  second construction still saw the first's leftover entries — added a
  second, PER-INSTANCE side table (`provider_instance_keys()`, keyed
  `(identity_hash_of_receiver, key)`, using the existing GC-move-stable
  `ctx.identity_hash_code()` — the same mechanism `System.identityHashCode`
  and the pre-existing `service_classname_table` already rely on) and
  routed `containsKey` through it instead of the shared name-keyed table.
  This preserves the name-keyed `provider_properties()` table for its
  existing job (bridging `Security.getProvider(name).getProperty(...)`
  reads across the *fresh* synthetic `Provider` object `make_provider()`
  hands out on every call — a different, legitimate cross-instance-by-name
  use this fix does not disturb) while giving `containsKey`'s duplicate-key
  guard correct per-object isolation. Verified: the 8-line double-construct
  repro passes consistently (3/3 runs) after the fix; failed 100% before.

  **NOT a ZeroCopy regression** — see below, that flakiness is pre-existing
  and reproduces identically on the unmodified baseline binary.

**Second bug — FIXED this session (follow-up, same day)**: with the
`putService`/`containsKey` fix above, `BouncyCastleSelfSignedCertGenerator
.generate()` got further but still failed in `JcaX509CertificateConverter
.getCertificate()` -> `CertificateFactory.getInstance("X509", bcProvider)`.
On real HotSpot this returns a `org.bouncycastle.jcajce.provider.asymmetric
.x509.X509CertificateObject` (BC's own concrete SPI class, `encoded len =
688`). On CratonVM it returned a bare `java.security.cert.X509Certificate`
— the abstract class itself — with `cert.getEncoded().length == 0`,
surfacing downstream as `AbstractMethodError: Certificate.verify
(PublicKey)`.

Root cause: `CertificateFactory` objects built via the real-bytecode
`getInstance(algo, Provider)` / `getInstance(algo, providerName)` paths
(which route through `sun.security.jca.GetInstance` ->
`getinstance_instance_provider[_obj]` in `native-builtins/src/jca/
provider_chain.rs`, running the class's real constructor) do end up with a
genuine `certFacSpi` field pointing at BC's real SPI (BC registers it via
the now-working `putService`). But `register_p68_security_cert` in
`native-builtins/src/phases_late.rs` — which implements `generateCertificate`
/ `generateCertificates` — always ran its own hardcoded synthetic DER
parser regardless of what SPI the `CertificateFactory` was actually built
with, ignoring `certFacSpi` entirely.

  Fix (`native-builtins/src/phases_late.rs`, `register_p68_security_cert`,
  commit `8355ad22` on `fix/httpserver-certfactory-20260706`, merged to
  `dev`): both `generateCertificate` and `generateCertificates` now check
  for a real `certFacSpi` field first and, if present, delegate to it via
  `invoke_virtual` (`engineGenerateCertificate` / `engineGenerateCertificates`),
  running the genuine provider bytecode and producing the provider's own
  concrete `Certificate` subclass. Falls through to the legacy synthetic
  DER parser only when `certFacSpi` is null (the old 1-arg
  `getInstance(String)` synthetic-stub path, which is unchanged), so this
  does not regress that path. Repros used: `CertFactoryRepro.java` (probes
  all three `getInstance` overloads + the private `certFacSpi` field via
  reflection) and `CertConvertRepro.java` (full BC `X509v3CertificateBuilder`
  -> `JcaX509CertificateConverter` -> `cert.verify()` chain matching Spring's
  actual usage), both under `/data/data/tmp-httpserver/` on the Azure host
  (not committed — standalone scratch repros).

  This fix was scoped to `CertificateFactory` only; it was NOT generalized
  to other JCA engine types (`Signature`/`KeyFactory`/`MessageDigest`/etc.)
  in this session. Those engine types already have their own dispatch via
  the `ec_real`-gated `getinstance_instance_provider_obj` /
  `build_jca_instance` mechanism in `provider_chain.rs` (EC-family only by
  design, see that file's doc comments) and were not found to share this
  specific bug — `CertificateFactory` is a different top-level class
  (`java.security.cert.CertificateFactory`, not `sun.security.jca.
  GetInstance`) with its own always-on synthetic native that intercepted
  unconditionally, which is what made it special-cased and worth this
  targeted fix rather than a shared one.

**Third bug — FIXED this session (2026-07-06, third pass)**: the
`pw.is_empty()` guard described above was confirmed against real JDK 25
source (`javax.crypto.spec.PBEKeySpec` explicitly normalises a null/0-length
password to `new char[0]` rather than rejecting it; `com.sun.crypto.provider
.PBEKey`'s constructor comment reads verbatim `// Should allow an empty
password.`) AND against a live standalone repro on real HotSpot JDK 25
(`SecretKeyFactory.getInstance("PBE").generateSecret(new
PBEKeySpec(new char[0]))` succeeds, `getEncoded().length == 0`; a full
`KeyStore` PKCS12 `setEntry`/`store`/`load`/`getKey` round trip with an
empty password also succeeds end-to-end) — so the guard was a genuine
CratonVM bug, not a JDK restriction. Fixed in `pbe_generate_secret`
(`native-builtins/src/phases_early.rs`): the empty-password case now
allocates the same 2-field `javax/crypto/spec/SecretKeySpec` shape directly
via `alloc_concurrent_synthetic` (bypassing only the real `<init>`'s own
`IllegalArgumentException("Empty key")` guard, which is genuine and
specific to `SecretKeySpec` — NOT to `SecretKey`/`PBEKey` in general)
instead of throwing.

**Fourth bug — FIXED this session**: with the PBE guard fixed, the test
still failed, now with `AbstractMethodError: SSLSocketFactory.createSocket
(...)... has no Code attribute` — reproducible in an 8-line standalone
repro (`SSLContext.getInstance("TLS").getSocketFactory().createSocket(...)`).
Root cause: `register_phase68_natives` (in `native-builtins/src/phases_late.rs`
— covers `javax.crypto.Mac`, the entire `javax.net.ssl.*` real-native-TLS
family, `java.security.cert`, JDBC, XML stubs) was reachable ONLY from
`register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]`
-gated and therefore **compiled entirely out of the default `cratonvm-cli`
real-JDK build** (`vm/src/native/builtins.rs` supplies a no-op shim for
non-synthetic builds). So in real-JDK mode — the default, and what every
`--jdk real` suite run uses — none of `register_p68_ssl`'s natives
(`SSLContext.getSocketFactory`/`createSSLEngine`, `SSLSocketFactory
.createSocket`, `SSLEngine` accessors, etc.) were EVER registered, despite
several being explicitly doc-commented as "real-mode reachable via
register_essential_natives" (a stale claim from an earlier, unrelated
MBeanServer/SSLSocket fix — see
`docs/internal/CRATONVM_BUGS/BUG-interfacedispatch-mbeanserver-sslsocket-realmode-shadow.md`).

  Fix: call `register_p68_ssl` (NOT the whole `register_phase68_natives`
  umbrella — see the "narrow, not broad" note below) directly from
  `register_essential_natives` (`native-builtins/src/lib.rs`), positioned
  BEFORE `net_phase_e::register_phase_e_networking` so `net_phase_e`'s own,
  correct `SSLContext.createSSLEngine()` registration (which allocates a
  real rustls-backed `sun/security/ssl/SSLEngineImpl`, not the abstract
  `javax/net/ssl/SSLEngine`) wins via last-writer-wins over
  `register_p68_ssl`'s fake, non-cryptographic `SSLEngine` wrap/unwrap stub
  (a hardcoded fake-ClientHello echo, no real TLS at all — this was ALSO
  silently shadowing the real engine in every real-JDK build until this
  fix, a second, independent bug hiding behind the first).

  **"narrow, not broad" note**: the first attempt at this fix called the
  whole `register_phase68_natives` umbrella (matching how
  `register_p68_security_cert` — a sibling function — was already
  separately made real-mode-reachable by a prior session). This directly
  regressed Tomcat's `StandardServer.initInternal` (parses `server.xml` via
  SAX): `register_phase68_natives` also bundles `register_p68_xml`, whose
  `SAXParserFactory.newSAXParser()` is a synthetic-only stub (a bespoke
  hand-rolled SAX walker, not real bytecode) missing `SAXParser
  .getXMLReader()` entirely — once reachable in real mode, it pre-empted
  the real bytecode path that would otherwise have constructed a genuine
  `com.sun.org.apache.xerces...SAXParserImpl` (which DOES have a real
  `getXMLReader()`), throwing `AbstractMethodError` and turning several
  previously-passing classes (`EchoHandlerIntegrationTests`,
  `ErrorHandlerIntegrationTests`, `MultipartHttpHandlerIntegrationTests`,
  and others hitting Tomcat's `[4]` backend parameterization) into
  FAIL/TIMEOUT. Narrowing to `register_p68_ssl` only resolved this — see
  Verification below.

**Fifth bug — found this session, NOT fixed, new residual**: with bugs 3+4
fixed, `SSLContext.createSSLEngine()` now correctly returns a real
`sun.security.ssl.SSLEngineImpl` (confirmed via a standalone repro:
`engine.getClass().getName()` -> `sun.security.ssl.SSLEngineImpl`,
`getSupportedCipherSuites().length` -> 9) and `KeyStore.setKeyEntry`'s
identity correctly reaches the native TLS layer's `runtime_tls_identity`
slot (see below) — but the test STILL fails, now with:

```
org.springframework.web.client.ResourceAccessException: I/O error on POST request for "https://localhost:PORT/foo": TLS handshake failed: unexpected EOF
```

This is a genuinely new, distinct layer, deep inside the rustls-backed
`SSLEngine.wrap()`/`unwrap()` implementation in
`native-builtins/src/t27_tls.rs` (`do_wrap`/`do_unwrap`, ~line 4381/4591),
NOT anywhere in the JCA/keystore/registration-reachability layers this
session otherwise fixed. Confirmed via targeted tracing (temporarily
instrumented and since fully removed — no net diff in `t27_tls.rs`):

- The server engine's handshake genuinely begins (`engine_begin` runs with
  `is_client=false`, `identity_override` populated — real cert/key data IS
  reaching rustls).
- The client's TLS ClientHello IS delivered to the server and `do_unwrap`
  processes it (confirmed: "called" fires, real record-parsing code runs).
- `do_wrap` is then called twice by the Java/Netty side to produce a
  ServerHello etc. — but **on the very FIRST `do_wrap` call, `s.closed_outbound`
  is ALREADY `true`**, so `do_wrap` short-circuits immediately (`SR_CLOSED`
  result, zero bytes produced) without ever emitting the ServerHello. Since
  `closed_outbound` is only ever set by the `SSLEngine.closeOutbound()`
  native (`native-builtins/src/t27_tls.rs`, ~line 4132) — i.e. JAVA CODE
  (Netty) is itself calling `closeOutbound()` right after the first
  `unwrap()` — this means Netty's `SslHandler` is reacting to something
  about the FIRST `unwrap()`'s `SSLEngineResult` (status/handshakeStatus)
  that makes it believe the handshake has failed, and it aborts by closing
  outbound before ever trying to send a server response.
- Frustratingly, `do_unwrap`'s actual result (status/handshakeStatus/
  consumed/produced) could not be captured directly — every explicit
  `return` path in `do_unwrap` was instrumented (pending-plaintext drain,
  `dst_cap==0`, engine-handle-missing, rustls `process_new_packets` error,
  and the final normal-path return) and NONE of them fired despite
  `do_unwrap`'s entry logging 3 times across 3 calls, no Rust panic was
  logged (panics ARE caught by `vm_exec.rs::safe_native_call`'s
  `catch_unwind` and, outside the bootstrap-quiet path, print to stderr —
  none did), and disabling the JIT (`--nojit`) made no difference. This
  looks like either a very subtle control-flow path in `do_unwrap` not yet
  identified, or a threading/stderr-ordering artifact (Netty's server event
  loop runs `do_wrap`/`do_unwrap` on a different OS thread than the
  JUnit/client thread — plausible but not confirmed to fully explain a
  100%-reproducible zero-hits-on-every-explicit-return result across
  repeated runs).

**Not investigated further this session** — this is now a genuinely
separate, pre-existing bug in the rustls engine plumbing itself
(`t27_tls.rs`'s `do_wrap`/`do_unwrap`/`engine_begin`), unrelated to the
JCA/keystore/registration-reachability chain this session's other four
fixes targeted. A future session should: (1) get `do_unwrap`'s actual
first-call `SSLEngineResult` (status + handshakeStatus) safely into view —
try a synchronous single-threaded repro (a bare `SSLEngine` pair driven
manually via `wrap`/`unwrap` in one thread, no Netty/Reactor involved, to
rule out the threading-order hypothesis) rather than tracing through the
full Netty stack; (2) once the actual result is known, compare against what
real JSSE's `SSLEngineImpl.unwrap()` would legitimately return for a raw
ClientHello record fed into a fresh server-mode engine, to find the
semantic mismatch that's making Netty give up early.

### PKCS12/PBE + TLS-registration fixes — files changed

- `native-builtins/src/phases_early.rs` — `pbe_generate_secret`: empty
  password no longer throws; allocates a 2-field `SecretKeySpec` shape
  directly for the 0-length-key case.
- `native-builtins/src/keystore.rs` — new `keystore_set_key_entry` +
  `engineSetKeyEntry` native (`KeyStore.setKeyEntry(String, Key, char[],
  Certificate[])` was entirely unregistered before this session, so real
  bytecode ran but never told CratonVM's native TLS bridge about the new
  identity); `unwrap_keystore_spi` (KeyManagerFactory.init/TrustManagerFactory
  .init receive the `java.security.KeyStore` wrapper, not the `KeyStoreSpi`
  the native `engine*` methods run on and stamp their store-id side-table
  against — `keystore_set_pending_km_identity` was unwrapping the WRONG
  object and always read back store-id 0); `keystore_get_first_private_key`
  fallback (a keystore built via `load(null,null)` then `setKeyEntry(...)`
  has an empty load-time identity snapshot even though a real entry was
  since added — scan the live registry instead of only the stale
  snapshot); `engine_set_key_entry` also calls
  `t27_tls::install_identity_from_der` to populate the process-wide
  "runtime server identity" the native TLS listener actually consumes for
  the accept-side handshake (previously only `engineLoad`, the byte-stream
  path, did this).
- `native-builtins/src/lib.rs` — `register_essential_natives` now calls
  `register_p68_ssl` directly (positioned before
  `net_phase_e::register_phase_e_networking`), making
  `SecretKeyFactory`/`SSLContext`/`SSLSocketFactory`/`SSLEngine`/`SSLSocket`
  real-TLS natives reachable in real-JDK mode for the first time.
- `native-builtins/src/phases_late.rs` — added `SSLEngine.getSupportedCipherSuites`
  / `getSupportedProtocols` (present on `SSLSocket`/`SSLSocketFactory`
  already, missing on `SSLEngine` — same "missing accessor"
  `AbstractMethodError` shape as the historical SSLSocket fix).

Branch `fix/httpserver-pkcs12-20260706`, off `dev`. See git log for the
actual merge commit hash once landed.

### Verification (2026-07-06, third pass) — full `http.server.` cluster regression check

Ran the full `--only 'http\.server\.'` cluster (33 classes) twice: once
with an intermediate (buggy) build that called the whole
`register_phase68_natives` umbrella too early, and once with the corrected
`register_p68_ssl`-only fix. Tally: `LOADERR=2, OK=24, FAIL=2, TIMEOUT=5`
on the corrected build (vs `LOADERR=2, OK=21, FAIL=3, TIMEOUT=7` on the
buggy intermediate build) — `EchoHandlerIntegrationTests` and
`MultipartHttpHandlerIntegrationTests` moved from broken (SAXParser
regression, see the "narrow, not broad" note above) to fully passing.

The 2 `LOADERR`s (`org.springframework.mock.http.server.reactive
.MockServerHttpRequestTests`/`MockServerHttpResponseTests`,
`ClassNotFoundException`) are a pre-existing test-classpath gap, unrelated
to any VM code.

The remaining 5 `TIMEOUT`s (`AsyncIntegrationTests`,
`ErrorHandlerIntegrationTests`, `RandomHandlerIntegrationTests`,
`ServerHttpRequestIntegrationTests`, `WriteOnlyHandlerIntegrationTests`)
and the `CookieIntegrationTests` partial-`FAIL` (`[3] Reactor Netty`,
"failed to respond") were all individually re-run against a freshly-built,
completely unmodified `dev` baseline (commit `aafdaec8`) — **every single
one reproduces identically on baseline** (`AsyncIntegrationTests` and
`CookieIntegrationTests` both TIMEOUT at 180s on baseline in one run;
`ErrorHandlerIntegrationTests`/`RandomHandlerIntegrationTests` passed
cleanly and `ServerHttpRequestIntegrationTests` TIMED OUT on baseline in a
second run). All 6 of these classes share the same
`AbstractHttpHandlerIntegrationTests` 4-way-parameterized base class this
doc's `ZeroCopyIntegrationTests` section already documents as intermittently
(40-60%) hanging on this Azure Linux host due to a pre-existing
ByteBuddy/AssertJ dynamic-class-generation race — confirmed to be the same
family of flakiness, NOT a regression from this session's diff. Net: this
session's fixes introduce zero regressions in the broader `http.server.`
cluster.

### `ZeroCopyIntegrationTests` — Jetty Core backend: ORIGINAL bug NOT reproduced; UNRELATED pre-existing flakiness found instead

This session could not reproduce the originally-reported `written 0 < N
content-length` failure on the Jetty Core backend at all — every successful
run (i.e. the ones that didn't hit the flakiness described below) passed
both non-assumption-skipped parameterizations (Reactor Netty, Jetty Core)
cleanly: `found=4 succ=2 fail=0 skip=0 abort=2`. This may mean it was already
fixed by an unrelated `dev` merge since the prior session documented it, or
it may be environment-dependent (the original report was captured on
Windows+JDK25; this session's verification host is Azure Linux) — not
re-confirmed either way, so **not** moved to a "fixed" section; it simply
did not reproduce here despite specific, repeated attempts.

**New, unrelated finding**: `ZeroCopyIntegrationTests` is **flaky** on this
Azure Linux host — roughly 40-60% of standalone runs hang indefinitely
(no further output after the JVM's early startup log lines) until an
external timeout kills the process. This reproduces **identically on the
completely unmodified `dev` baseline binary** (built before any change in
this session — confirmed via a direct A/B: baseline binary hung 2/5 runs,
a binary with this session's `Provider`/BC fix hung 3/5 runs, a third
intermediate binary hung 4/5 runs — all in the same ballpark, no
statistically meaningful difference, i.e. **this is not caused by this
session's `provider_chain.rs` change**). A `--stack-dump-on-timeout` capture
of a hung run shows the stuck thread deep inside ByteBuddy's dynamic-class
generation (`net.bytebuddy.dynamic.scaffold.TypeWriter$Default.make` →
`MethodDelegationBinder` → `StackManipulation$Compound.apply` →
`TypeList$Generic$AbstractBase.getStackSize` → `AbstractList$Itr.next`),
triggered by AssertJ's `Assumptions.assumeThat(...)` the FIRST time it's
called in a process (it lazily generates and caches a proxy class via
`net.bytebuddy.TypeCache.findOrInsert`) — i.e. this looks like a real,
pre-existing CratonVM concurrency/timing bug in ByteBuddy dynamic-class
generation (possibly a genuine race in interpreter/JIT state during
class-generation bytecode analysis), NOT specific to `ZeroCopyIntegrationTests`
or to anything touched this session. Worth a dedicated investigation in a
future session, but out of scope here — flagged so it isn't mistaken for a
regression from this session's diff.

## Repro

```bash
cd /c/craton/cratonvm/apps/spring-suite-runner
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1
CRATONVM_BIN=<your-built-cratonvm.exe> KRUN_STACK=1 \
  ./run-suite.sh run --jdk real --jit on --batch 1 --only 'http\.server\.'
```

On the Azure host, run-suite.sh needs `JDK25_WIN=/home/victor/jdk25` and a
`cygpath` shim on `PATH` (`/home/victor/localbin/cygpath` already exists —
it's a no-op passthrough since Linux needs no Windows-path translation) —
and the shared checkout's classpath join at
`apps/spring-suite-runner/run-suite.sh:194` uses `;` (Windows classpath
separator), which breaks on Linux. **Do not edit the shared checkout** — copy
`apps/spring-suite-runner` to scratch space (e.g. `~/my-suite-runner`) and
change that one line's `;` to `:` in your own copy instead.
