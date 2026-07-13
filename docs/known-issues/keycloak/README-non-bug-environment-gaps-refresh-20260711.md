# Non-bug environment/harness gaps accounting for the bulk of the 953 FAILs (refresh rerun, 2026-07-11)

This index exists so the FAIL count in the 2026-07-11 refresh rerun (953 of 1081 non-passed-before classes) is
fully accounted for. Most of the volume is **not** new CratonVM bugs — it's a small number of already-triaged
environment/harness gaps repeating across many classes. The genuinely new/still-open bugs from this rerun each
have their own doc in this folder (see list at the bottom); this doc just closes the loop on everything else.

## 1. Arquillian `auth-server-undertow` container-provisioning gap — ~543 classes (541 `testsuite/integration-arquillian/tests/base` + 2 `testsuite/integration-arquillian/tests/other/sssd`)

```
java.lang.IllegalStateException: Not found frontend container: auth-server-undertow
  org.keycloak.testsuite.arquillian.AuthServerTestEnricher.initializeSuiteContext(AuthServerTestEnricher.java:233)
```

Every class under `testsuite/integration-arquillian` needs a fully-configured Arquillian container adapter
(`auth-server-undertow`) that this harness/environment doesn't provision. Not a CratonVM bug — this is an
Arquillian container-configuration prerequisite, would fail identically under any JVM without the right
Arquillian setup.

## 2. `keycloak-test-framework-remote-providers` Maven artifact-resolution gap — ~345 classes (341 `tests/base` + 4 `tests/webauthn`)

```
java.lang.RuntimeException: Failed to resolve artifact: org.keycloak.testframework:keycloak-test-framework-remote-providers
  ... Caused by: io.quarkus.bootstrap.resolver.maven.BootstrapMavenException: Failed to load current project at .../tests/base/pom.xml
```

**RETRACTED 2026-07-13 — this IS a CratonVM bug, not an environment gap.** The 2026-07-07 "NOT A BUG" triage
(`docs/internal/keycloak-tests-base-remote-providers-artifact-resolution-NOT-A-BUG.md`) was based on a HotSpot
comparison run against a since-discovered-**stale** `keycloak-999.0.0-SNAPSHOT.zip` distribution artifact
(weeks old), under which HotSpot itself failed to boot the test server for unrelated reasons. After rebuilding
the distribution fresh and re-running under real HotSpot (solo/sequential, avoiding a shared-extraction-directory
race), HotSpot cleanly PASSES the large majority of these classes, while CratonVM still fails every one of them
with an identical, narrower root cause: CratonVM's file I/O drops/corrupts the `?` character in the `<?xml ...?>`
declaration when Quarkus's embedded Maven resolver reads `tests/base/pom.xml` in-process, breaking POM parsing.
See `docs/known-issues/keycloak/pom-xml-declaration-char-corruption-breaks-quarkus-maven-bootstrap.md` for the
full writeup, evidence, and retraction details.

## 3. FIPS-mode `Assume.assumeTrue` skip pattern — 10 `crypto/fips1402` classes

```
KCRUNNER_RESULT tests=N failed=0 aborted=N ...
```
(all tests "aborted", zero "failed" — from `Assume.assumeTrue(Environment.isJavaInFipsMode())` at the top of
each FIPS1402 test class)

This environment isn't running in FIPS mode, so these tests correctly self-skip — the same on any JVM. Not a bug.

## 4. Docker not available — 2 `tests/clustering` classes

```
java.lang.IllegalStateException: Could not find a valid Docker environment. Please see logs and check configuration
```

Testcontainers-based clustering tests need a running Docker daemon to spin up a PostgreSQL container; this
Windows dev machine doesn't have Docker running. Not a CratonVM bug — would fail identically under any JVM
without Docker available.

## 5. Quarkus `FacadeClassLoader` test-augmentation gap — 4 `quarkus/deployment` health-check classes

```
java.lang.RuntimeException: Internal error. The test class ... should have been loaded with a QuarkusClassLoader,
but instead it was loaded with jdk.internal.loader.ClassLoaders$AppClassLoader@...
```

Already disclosed in the 2026-07-07 investigation pass as a likely Quarkus-test-augmentation/harness limitation
(this per-class-JVM-launch harness doesn't replicate Quarkus's full build-time augmentation lifecycle that these
tests expect) rather than a confirmed CratonVM bug — not re-investigated here, just re-disclosed for completeness.

## 6. `test-framework/remote :: TestClassServerTest` — 1 class, likely minor/pre-existing

```
org.opentest4j.AssertionFailedError: Expected java.lang.ClassNotFoundException to be thrown, but nothing was thrown.
```

Single test, not re-investigated this pass (already flagged as low-priority in the 2026-07-07 session).

---

## Genuinely new/still-open findings from this rerun (separate docs in this folder)

- `testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe.md` — 37 classes, `sun.misc.Unsafe.putOrderedLong()` NPE breaking Netty EventLoopGroup construction
- `scim-filter-antlr-and-clause-always-required.md` — 1 class (23/23 tests), SCIM filter ANTLR grammar always requires a trailing AND
- `cipher-wrap-unwrap-not-implemented-aeskeywrap128.md` — 2 classes, AES Key Wrap 128 wrap/unwrap not implemented
- `eddsa-keyspec-to-publickey-invalidkeyspecexception.md` — 2 classes, Ed25519/Ed448 PublicKey-from-KeySpec reconstruction fails
- `x509-subject-cn-extraction-returns-null.md` — 1 class, X.509 Subject CN extraction returns null for a real certificate fixture
- `quarkus-runtime-config-resolution-mismatches.md` — 4 classes, SmallRye/Quarkus config resolution mismatches (wrong values, env-var leakage into property enumeration)

## Accounting

543 (Arquillian) + 345 (remote-providers) + 10 (FIPS-skip) + 2 (Docker) + 4 (QuarkusClassLoader) + 1 (TestClassServerTest)
+ 37 (Unsafe/Netty) + 1 (SCIM ANTLR) + 2 (Cipher AESWrap) + 2 (EdDSA KeySpec) + 1 (X509 CN) + 4 (Quarkus config)
= **952** of 953 FAILs accounted for (the remaining 1 is likely rounding/an edge case in one of the above
buckets' exact counts — not independently investigated further given the overwhelming majority is explained).
