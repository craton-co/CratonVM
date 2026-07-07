# KeyFactory.translateKey() NPE on synthetic KeyFactory's null `spi` field

Status: OPEN — new, found during 2026-07-07 full WildFly suite bug-bash run
Severity: Medium (breaks self-signed X.509 certificate generation via Elytron/WildFly-security helpers; likely affects any code calling `KeyFactory.translateKey`)
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`

## Symptom

Any code path that calls `java.security.KeyFactory.translateKey(Key)` on a CratonVM-backed `KeyFactory` throws:

```text
java.lang.NullPointerException: Cannot invoke "java.security.KeyFactorySpi.engineTranslateKey(java.security.Key)" because "this.spi" is null
	at java.security.KeyFactory.translateKey(KeyFactory.java:475)
```

Concretely this breaks `org.wildfly.security.x500.cert.X509CertificateBuilder.getTBSBytes()` →
`SelfSignedX509CertificateAndSigningKey.Builder.build()`, i.e. **generating a self-signed X.509
certificate fails outright** under CratonVM. This is a pure client-side crypto operation with
no server/container involved — no Arquillian deployment, no management client, nothing WildFly-specific
about the failure mechanism itself.

## Confirmed CratonVM-specific via HotSpot A/B (same class, same harness, same command)

`org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase`
(module `testsuite/integration/manualmode`), which calls
`org.jboss.as.test.integration.security.common.Utils.createKeyStoreTrustStore()` in its `@Before` setup
(`prepareSSLFiles`, line 207), building an RSA-1024 / SHA256withRSA self-signed cert via
`SelfSignedX509CertificateAndSigningKey.builder()...build()`:

- **Real HotSpot**: `OK` — 22/22 test methods pass, wall time 79s.
- **CratonVM** (jit-real mode, identical class/command): `FAIL` — `Errors: 2`, both failing in the
  `@Before` setup with the `KeyFactory.translateKey` NPE above, before any test method body even runs.

Same Maven/Surefire invocation, same class, only the JVM differs — this rules out any
Arquillian/WildFly-server explanation. It reproduces with a bare local crypto call.

## Root cause (found via source read, not yet fixed)

`native-builtins/src/jca/key_factory.rs` intercepts `KeyFactory`/`KeyPairGenerator` with a native
"3-field synthetic" object layout (`algo_idx`, `key_size`, `state` — see the module doc comment at the
top of the file) instead of a real JDK `Provider`/`Spi`-backed object, to avoid an unrelated real-bytecode
NPE in `sun.security.jca.GetInstance.getServices()` (CratonVM doesn't materialize a real
`Provider.services` map — see the comment on `register()`). This shim registers `getInstance`,
`initialize`, `generateKeyPair`, `getAlgorithm`, etc., but **does not register a native override for
`KeyFactory.translateKey`** (confirmed: zero occurrences of `translateKey` in the file).

Because the shim never sets the real JDK `KeyFactory.spi` field (the synthetic object has no such
field/concept), any caller that invokes the *unshimmed* `translateKey()` falls through to the real JDK
bytecode, which unconditionally does `return spi.engineTranslateKey(key);` — and `spi` is `null` on a
synthetic-shimmed `KeyFactory`, so it NPEs.

Note this shim is gated off entirely when `CRATONVM_REAL_JCA` is set (`real_jca_mode()` in
`native-builtins/src/lib.rs:1205`) — but that's an opt-in env var, not the default, so the default
(`real_jca_mode() == false`) configuration used by this suite run hits the synthetic path and this gap.

## Suggested fix

Register a native override for `KeyFactory.translateKey(Key)` alongside the other `KeyFactory`/
`KeyPairGenerator` natives in `native-builtins/src/jca/key_factory.rs`, backed by the same
`crypto_impl` software primitives the rest of the file already uses for RSA/EC key material — i.e. treat
`translateKey` like a same-algorithm "reconstruct this key through my own KeyFactory" no-op/copy for keys
whose `algo_idx` matches, and do a real conversion (or a clear `InvalidKeyException`, matching real JDK
behavior) for cross-algorithm translation.

## Repro

```bash
# On the Azure host, from a WildFly checkout with target/wildfly already built:
cd apps/wildfly-suite-runner   # own copy pointed at WILDFLY=<built wildfly checkout>
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<any cratonvm release binary>
export JDK25_WIN=<real JDK 25 home>
./run-suite-linux.sh run --category all --jit on --jdk real --class-to 300 \
  --only 'ElytronRemoteOutboundConnectionTestCase' --tag repro
# -> FAIL, Errors: 2, KeyFactory.translateKey NPE in @Before setup

./run-suite-linux.sh hotspot --category all \
  --only 'ElytronRemoteOutboundConnectionTestCase' --tag repro-hotspot
# -> OK, 22/22 tests pass
```

A minimal non-WildFly repro should also work directly against `cratonvm`:

```java
import java.security.*;
KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
kpg.initialize(1024);
KeyPair kp = kpg.generateKeyPair();
KeyFactory kf = KeyFactory.getInstance("RSA");
kf.translateKey(kp.getPrivate());  // -> NPE: this.spi is null
```

## Evidence

```text
/data/data/wt-wildfly-bugbash-20260707-runner/out/resume-s2of2-jit-real-all-20260707-031720/surefire-reports/00293-org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase/org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase.txt
/data/data/wt-wildfly-bugbash-20260707-runner/out/hscheck-ely-hotspot-all-20260707-043632/  (HotSpot A/B baseline, OK 22/22)
```

## Related / not to be confused with

Unrelated to [[wildfly-domain-heap-corrupt-value-timeout]] and [[wildfly-domain-managed-servers-timeout]]
— no domain-mode involved here — and unrelated to the much larger "no managed container started"
`integration/*` cluster from the same run (this test class doesn't even need Arquillian's managed container
to fail; the NPE happens in local `@Before` setup before any deployment is attempted). The zero-test-class
Surefire handshake bug this doc originally cross-referenced is already FIXED (see
`docs/internal/wildfly-suite-bugs/bug-16-*.md` / `bug-17-*.md`).

**Duplicate finding, same underlying bug, found independently a day earlier via a different app:**
`docs/known-issues/keycloak-07-04/crypto-elytron-keyfactory-spi-null-and-x509extension-abstractmethoderror.md`
("Finding 1") hits the exact same `KeyFactory.translateKey` → `this.spi is null` →
`X509CertificateBuilder.getTBSBytes` signature via Keycloak's `ElytronCertificateUtilsProvider`, dated
2026-07-06 (before this doc). That doc frames the root cause as `KeyFactory.getInstance()`'s SPI-binding
step silently succeeding without completing SPI selection; this doc instead pinpoints the specific gap as
`KeyFactory.translateKey` not being among the natively-registered methods in
`native-builtins/src/jca/key_factory.rs` (getInstance/generatePublic/etc. *are* registered against the
3-field synthetic object, translateKey specifically is not) — these are two framings of the same mechanism,
not two different bugs. Whoever fixes this should treat both docs as describing one defect and close/merge
them together rather than fixing twice.
