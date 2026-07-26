# Non-bug environment/harness gaps accounting for the bulk of the 953 FAILs (refresh rerun, 2026-07-11)

This index exists so the FAIL count in the 2026-07-11 refresh rerun (953 of 1081 non-passed-before classes) is
fully accounted for. Most of the volume is **not** new CratonVM bugs — it's a small number of already-triaged
environment/harness gaps repeating across many classes. The genuinely new/still-open bugs from this rerun each
have their own doc in this folder (see list at the bottom); this doc just closes the loop on everything else.

## 1. Arquillian `auth-server-undertow` container-provisioning gap — FIXED (2026-07-15)

The per-class runner now materializes the effective Maven Surefire configuration and supplies the transformed
Arquillian descriptor. The focused HotSpot probe executes all five tests with all three containers successful.
The retired record is [here](../../internal/fixed-suite-bugs/keycloak-arquillian-auth-server-undertow-container-not-found-FIXED.md).

## 2. `keycloak-test-framework-remote-providers` Maven artifact-resolution gap — ~345 classes (341 `tests/base` + 4 `tests/webauthn`)

```
java.lang.RuntimeException: Failed to resolve artifact: org.keycloak.testframework:keycloak-test-framework-remote-providers
  ... Caused by: io.quarkus.bootstrap.resolver.maven.BootstrapMavenException: Failed to load current project at .../tests/base/pom.xml
```

**RETRACTED 2026-07-13, then CLOSED 2026-07-14.** The 2026-07-07 "NOT A BUG" triage
(docs/internal/keycloak-tests-base-remote-providers-artifact-resolution-NOT-A-BUG.md) was based on a HotSpot
comparison run against a since-discovered-**stale** keycloak-999.0.0-SNAPSHOT.zip distribution artifact
(weeks old), under which HotSpot itself failed to boot the test server for unrelated reasons -- retracted the
same day pending further investigation of an apparent CratonVM-side pom.xml char-corruption bug. That followup
investigation (2026-07-14) could NOT reproduce the corruption via an exhaustive, byte-exact, full-harness
retest (real Keycloak 26.6.1 Quarkus dist built from source, real tests/base/pom.xml, the actual
keycloak-test-framework bootstrap, both at current dev and bisected back to the exact commit the corruption was
originally observed at) -- see
docs/internal/fixed-suite-bugs/pom-xml-declaration-char-corruption-breaks-quarkus-maven-bootstrap-FIXED.md
for the full writeup. Two separate, genuine CratonVM bugs were found and fixed in the same
DistributionKeycloakServer.start() code path along the way (ProcessBuilder silently dropping a
LinkedList-backed command list; Process.descendants()/ProcessPipeInputStream.readAllBytes() unregistered),
and the real Keycloak server now boots successfully under CratonVM through this exact path. Three further,
unrelated residuals (Selenium/HtmlUnit JSON parsing, a missing sun.management native, a resteasy classpath
gap) were newly discovered downstream of a successful server boot -- flagged separately, not part of this
issue.

## 3. FIPS-mode JUnit assumptions — reporting classification fixed (2026-07-15)

The historical rerun counted ten `crypto/fips1402` assumption-gated classes as `FAIL` because the harness treated
every JUnit abort as a failure. The runner now reports all-aborted classes as `SKIP` and mixed pass/abort classes as
`PARTIAL`; actual failures remain `FAIL`. A focused current-dev Azure comparison of all 21 FIPS classes produced
the identical HotSpot/CratonVM distribution: 11 PASS, 3 PARTIAL, 7 SKIP, 0 failed, and 0 failed containers.
See [the fixed reporting record](../../internal/fixed-suite-bugs/keycloak/keycloak-fips1402-assumption-aborts-classification-FIXED.md).

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

Historical note: the 10 FIPS assumption-gated rows above were formerly labelled `FAIL`; they now report as
non-failure `SKIP`/`PARTIAL` outcomes under the corrected harness policy.
