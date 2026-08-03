# Fixture gap: BouncyCastle jars missing from the Windows suite classpath — 3 classes

| | |
|---|---|
| **Status** | Fixture gap, NOT a CratonVM bug |
| **HotSpot** | Fails identically for the BC-dependent methods (same classpath) |
| **Discovered** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |

## Symptom

```
1) testClientMLDSAwithMLDSAServer[JSSE](org.apache.tomcat.util.net.TestPQC)
java.lang.NoClassDefFoundError: org/bouncycastle/jce/provider/BouncyCastleProvider
```

```
1) testLargeClientHelloWithSessionResumption(org.apache.tomcat.util.net.TestLargeClientHello)
java.lang.NoClassDefFoundError: org/bouncycastle/asn1/x500/X500Name
```

```
1) testCVE_2018_8034(org.apache.tomcat.security.TestSecurity2018)
java.lang.Exception: Unexpected exception, expected<jakarta.websocket.DeploymentException> but was<java.lang.NoClassDefFoundError>
Caused by: java.lang.NoClassDefFoundError: org/bouncycastle/asn1/x500/X500Name
```

`TestPQC` (post-quantum ML-DSA/ML-KEM key exchange), `TestLargeClientHello`
(large-certificate-chain session resumption), and `TestSecurity2018`
(CVE-2018-8034 renegotiation regression test) each need BouncyCastle on the
classpath to construct their test certificates/providers.

## Not a CratonVM bug

Already independently noted in
`docs/internal/fixed-suite-bugs/tomcat/28-http2-largeupload-byte-mismatch-FIXED.md`:
"`TestPQC`, `TestOcspEnabled` — FAIL on HotSpot too." A missing third-party
jar fails identically on any JVM.

Note `TestSecurity2018` and `TestLargeClientHello` are otherwise-passing
classes where only the BC-dependent method(s) fail — not a full-class
failure like `TestPQC`.

## Fix

Add `bcprov`/`bcpkix` (BouncyCastle provider + PKIX, matching the version
Tomcat's own build resolves) to `apps/tomcat/.suite/cp.txt` on Windows. Once
fixed, re-run these 3 classes to confirm no genuine CratonVM defect underneath
(`TestOcspEnabled` is a separate, already-noted flake — see
`docs/internal/fixed-suite-bugs/tomcat/serversocket-bind-socketaddress-noop-localport-zero-FIXED.md`,
which found it and `TestSsl` "flip between" pass/fail on both HotSpot and
CratonVM — not this classpath gap).
