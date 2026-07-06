# http.server bug cluster (12 classes) — fixes landed + residuals

## Status

**Mostly fixed** on branch `fix/http-server-cluster` (off dev `81762470`),
merged to `dev`. Reproduced and iterated on both Windows + JDK25 (the
platform the original bug report was captured on, via
`apps/spring-suite-runner`) and the Azure Linux host for fast iteration.
10 of 12 classes are fully fixed; 2 have residual issues, updated across
two 2026-07-06 follow-up passes below. `ServerHttpsRequestIntegrationTests`:
two distinct bugs found and fixed this session (a `Provider.putService`
gap + per-instance `containsKey` isolation, and a `CertificateFactory`
real-SPI delegation gap), but the test still fails — a third, unrelated
bug (PKCS12/PBE empty-password handling in Netty's JDK-native SSL context
path) was uncovered once the first two were fixed, and remains open for a
future session. `ZeroCopyIntegrationTests`'s original reported failure did
not reproduce, but an unrelated pre-existing flakiness was found and
documented instead — see "Update (2026-07-06 session)" below.

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

### `ServerHttpsRequestIntegrationTests` — TWO bugs found+fixed this session, ONE new bug remains open

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

**Third bug — found this session, NOT fixed, new residual**: with the
`certFacSpi` delegation fix above, the `AbstractMethodError` / empty-cert
failure is gone — confirmed via the standalone repros and a live run of
`ServerHttpsRequestIntegrationTests` — but the test still fails, now
further down the chain, inside Netty's **JDK-native** SSL context setup
(`JdkSslServerContext`, a different code path from BC's own SSL context —
this one builds an in-memory PKCS12 keystore via the JDK's own
`sun.security.pkcs12.PKCS12KeyStore`):

```
reactor.core.Exceptions$ReactiveException: javax.net.ssl.SSLException: failed to initialize the server-side SSL context
  at io.netty.handler.ssl.JdkSslServerContext.newSSLContext(JdkSslServerContext.java:350)
Caused by: java.security.KeyStoreException: Key protection algorithm not found: java.security.UnrecoverableKeyException: Encrypt Private Key failed: getSecretKey failed: Empty password
  at sun.security.pkcs12.PKCS12KeyStore.setKeyEntry(PKCS12KeyStore.java:719)
Caused by: java.security.UnrecoverableKeyException: Encrypt Private Key failed: getSecretKey failed: Empty password
Caused by: java.io.IOException: getSecretKey failed: Empty password
  at sun.security.pkcs12.PKCS12KeyStore.getPBEKey(PKCS12KeyStore.java:851)
Caused by: java.security.spec.InvalidKeySpecException: Empty password
```

Not investigated further this session — out of scope (this is a distinct,
unrelated bug from the `CertificateFactory` one this session targeted).
Hypothesis for a future session: CratonVM's PBE/PKCS12 key-protection
native path likely doesn't handle an empty-password `PBEKey` derivation the
way real HotSpot's `PKCS12KeyStore.getPBEKey`/`encryptPrivateKey` does —
worth confirming first whether real HotSpot JDK 25 actually accepts an
empty password here at all (Netty's in-memory keystore construction may
rely on specific PBE parameters CratonVM's crypto natives don't yet
support), before assuming it's a CratonVM-only gap. **Net effect:
`ServerHttpsRequestIntegrationTests` still FAILS**, but the failure has
moved twice now and is much narrower than the original report — a future
session should start at CratonVM's PKCS12/PBE key-protection natives (grep
for `PKCS12` / `PBEKey` / `getSecretKey` handling) rather than anywhere
in the JCA provider/service-lookup layer (which is now confirmed working
correctly for this test's `CertificateFactory` usage).

The original "all-NUL PEM payload" / base64 / file-I/O leads from the prior
session are now believed to be **red herrings** — with BC actually reachable
(once `putService` exists), the failure never gets far enough to reach any
PEM-encoding or file-I/O step; it fails earlier, inside certificate-object
construction itself.

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
