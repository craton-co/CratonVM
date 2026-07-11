# TestKeyManagerWrappingFips — bare assertion failure (FIXED)

Formerly tracked at `docs/known-issues/tomcat-08-07/keymanagerwrappingfips-bare-assertion.md`
(removed from the tracked tree now that this is resolved).

## Original symptom

`org.apache.tomcat.util.net.TestKeyManagerWrappingFips.testBug64614_01` failed
with a bare `AssertionError` on `Assert.assertFalse` — PASS on HotSpot.

## Root cause

`javax.net.ssl.KeyManagerFactory.getInstance(String algorithm)` was a hardcoded
native stub (`native-builtins/src/phases_late.rs`'s `register_p68_ssl`) that
**always** attached the built-in synthetic `"SunJSSE"` provider to the returned
`KeyManagerFactory`, regardless of the requested algorithm and regardless of
any provider a caller registered via `Security.addProvider` +
`Provider.put("KeyManagerFactory.<algo>", ...)`.

The test registers a dummy provider (`FIPS_PROVIDER`) whose `getInfo()`
contains `"FIPS"` and a `KeyManagerFactory.DUMMY_ALGORITHM` service on it, then
calls `KeyManagerFactory.getInstance(DUMMY_ALGORITHM)`. Tomcat's
`SSLUtilBase.getKeyManagers()` branches on
`kmf.getProvider().getInfo().contains("FIPS")` to skip key-wrapping entirely
in FIPS mode. Because `getInstance` always returned the SunJSSE placeholder
(whose info string never contains `"FIPS"`), the branch was never taken, so
the wrapping path ran and set `DummyKeyStoreSpi.wrappingOccurred = true`,
failing `assertFalse(wrappingOccurred)`.

Confirmed directly with a minimal repro (`Provider` + custom
`KeyManagerFactorySpi`, no Tomcat involved): `kmf.getProvider()` returned a
*different* object than the one just registered, named `"SunJSSE"`, with
CratonVM's own placeholder info string instead of the caller's.

## Fix

`native-builtins/src/jca/provider_chain.rs`: added
`find_service_provider(type, algo)` (search the provider chain — which
already includes providers added via `Security.addProvider` — for who
registered a matching service) and `resolve_or_make_provider(ctx, name)`
(prefer the REAL user `Provider` object on file via `real_provider_table` so
`getInfo()`/`getName()` return exactly what the caller's own constructor set,
falling back to a synthetic for seeded/unknown names).

`native-builtins/src/phases_late.rs`'s `KeyManagerFactory.getInstance(String)`
native now calls `find_service_provider("KeyManagerFactory", algorithm)`
instead of hardcoding `"SunJSSE"`, falling back to `"SunJSSE"` only when no
provider in the chain registered the requested algorithm — preserving every
existing `SunX509`/`NewSunX509`/`PKIX` caller (those are still seeded under
SunJSSE, so the lookup resolves to the same provider as before).

`factorySpi` (field 1) is left `None`, unchanged from before — nothing reads
it (verified via grep), and both tests in this class only exercise
`getProvider()`, not `getKeyManagers()`'s return value.

## Verification

- Minimal repro (custom `Provider` + `KeyManagerFactorySpi`, no Tomcat):
  `kmf.getProvider()` now returns the exact registered object; `getInfo()`
  matches HotSpot's output exactly.
- `org.apache.tomcat.util.net.TestKeyManagerWrappingFips` (both
  `testBug64614_01` and `testBug64614_02`): `OK (2 tests)`.
- Regression sweep (166 tests: `TestClientCert`, `TestCustomSslTrustManager`,
  `TestSslHandshakeFailure`, `ocsp.TestOcspEnabled`, `ocsp.TestOcspSoftFail`,
  `security.TestSecurity2017Ocsp`) — identical 13 pre-existing failures
  before and after (same failing test names), confirming zero regressions.
  Those 13 are a separate, pre-existing client-cert/OCSP handshake issue
  unrelated to this fix.

Fixed on branch `fix/keymanagerfactory-provider-lookup-20260711`, merged to
`dev`.
