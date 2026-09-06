# `EmbeddedLdapAutoConfigurationTests.whenSslBundleIsConfiguredLdapsListenerIsConfigured` — no legacy-DSA TLS backend on Windows

**Status: OPEN — accepted platform limitation, 2026-07-31.** Windows only; the
class passes 17/17 on Linux.

## 2026-08-11 — the two load-bearing claims are now measured, not inferred, and the message names the key type

This page's root-cause section reasons that a DSA certificate needs a `DHE_DSS`
suite and that modern Windows SChannel no longer offers one. Both were
inferences. Both were checked directly, on the Windows suite host:

* **What HotSpot actually negotiates for this listener.** Running the single
  failing test under Temurin 25.0.3 with `-Djavax.net.debug=ssl:handshake`:

  ```
  "cipher suite" : "TLS_DHE_DSS_WITH_AES_256_GCM_SHA384(0x00A3)"
  ```

  So the requirement really is a `DHE_DSS` suite, and rustls — which implements
  no DHE key exchange at all — cannot reach it by any configuration.

* **What SChannel offers.** `Get-TlsCipherSuite` on this Windows 11 host lists
  **28 suites, of which 0 are DSS**. That settles the remaining "would feeding
  `native-tls` a PKCS#12 instead of `from_pkcs8` help?" question: no. The import
  failure (`CRYPT_E_ASN1_BADTAG`) is not the binding constraint — even a
  successful import could not negotiate a handshake afterwards.

Also measured: the key CratonVM lifts out of the fixture keystore is 335 bytes
of PKCS#8 whose `AlgorithmIdentifier` OID is `1.2.840.10040.4.1` (`id-dsa`),
captured with `CRATONVM_DBG=tls-hs`. The bytes are identical on Linux, where the
OpenSSL fallback accepts them and the class passes 18/18 — so the two platforms
differ only in the fallback, exactly as this page says.

**What changed in code (2026-08-11):** the exception text only. It used to be
rustls's generic "failed to parse private key as RSA, ECDSA, or EdDSA" followed
by a localized Win32 ASN.1 error, which reads like a corrupt keystore. It now
names the key algorithm and the missing capability, so the next reader of a
Windows suite log does not have to re-derive this page. The gap itself is
unchanged and the status stays OPEN for the reason given below: the fix is an
OpenSSL dependency on Windows, not a VM change.

## Reconfirmed 2026-08-07, Windows full-suite (`craton-fullsuite-windows-20260806`)

Triaging the 08-06/07 Windows full-suite run's non-passing classes turned this
class up again: `module/spring-boot-ldap`'s
`EmbeddedLdapAutoConfigurationTests` FAIL, 1/17, 27.449s
(`craton-fullsuite-windows-20260806-s3/all-jit/logs/module_spring-boot-ldap.org.springframework.boot.ldap.autoconfigure.embedde-3c6a8c4df0c6.{out,err}.log`).

Byte-for-byte the same signature this doc already documents — same failing
test, same `SBRUNNER_RESULT tests=17 failed=1`, same `directoryServer` bean
creation failure, same
`IOException(ServerConfig with_single_cert failed: unexpected error: failed
to parse private key as RSA, ECDSA, or EdDSA; platform TLS fallback: ...)`,
same `os error -2146881269` (`CRYPT_E_ASN1_BADTAG`) — this run's console
locale renders the OS message in Russian ("Встречено неверное значение тега
ASN1", i.e. "an invalid ASN1 tag value was encountered") but the error code is
identical. Confirms this is still the same accepted DSA/Windows-SChannel gap,
not a new regression; no doc changes needed beyond this confirmation.

## Reconfirmed 2026-08-10 — collector-agnostic (Generational, G1, and ZGC)

Reconciling the 139-class union of non-passed classes from the three
2026-08-08f full-suite reruns (`craton-nonpassed-{default,g1,zgc}-20260808f`)
on `dev@6365de194`, `module/spring-boot-ldap` `EmbeddedLdapAutoConfigurationTests`
is FAIL under **all three** GC backends, byte-identical signature in every
run: 1 of 17 tests failed
(`whenSslBundleIsConfiguredLdapsListenerIsConfigured`), same
`directoryServer` `BeanCreationException` chain, same
`IOException(ServerConfig with_single_cert failed: unexpected error: failed
to parse private key as RSA, ECDSA, or EdDSA; ...)`, same `os error
-2146881269` (`CRYPT_E_ASN1_BADTAG`):

| GC | Shard | Seconds | Log |
|---|---|---:|---|
| default (Generational) | s2 | 34.225 | `craton-nonpassed-default-20260808f-s2/all-jit/logs/module_spring-boot-ldap.org.springframework.boot.ldap.autoconfigure.embedd-3c6a8c4df0c6.{out,err}.log` |
| G1 | s2 | 38.808 | `craton-nonpassed-g1-20260808f-s2/all-jit/logs/module_spring-boot-ldap.org.springframework.boot.ldap.autoconfigure.embedded.Em-3c6a8c4df0c6.{out,err}.log` |
| ZGC | s2 | 35.949 | `craton-nonpassed-zgc-20260808f-s2/all-jit/logs/module_spring-boot-ldap.org.springframework.boot.ldap.autoconfigure.embedded.E-3c6a8c4df0c6.{out,err}.log` |

(all under `apps/spring-boot-suite-runner/.suite/results/`). This is the same
1024-bit-DSA-key/Windows-SChannel platform gap across all three collectors,
not a GC-specific defect — expected, since the root cause is a TLS
key-type/backend limitation on Windows with nothing to do with the
collector. Status and conclusion below unchanged: still OPEN, still an
accepted platform limitation.

## Symptom

`module/spring-boot-ldap`'s
`org.springframework.boot.ldap.autoconfigure.embedded.EmbeddedLdapAutoConfigurationTests`
fails 1 of 17 on Windows:

```
BeanCreationException: Error creating bean with name 'directoryServer' ...
  Factory method 'directoryServer' threw exception with message:
  An error occurred while attempting to start listener 'ldaps':
  IOException(ServerConfig with_single_cert failed: unexpected error: failed to parse
  private key as RSA, ECDSA, or EdDSA; platform TLS fallback:
  <CRYPT_E_ASN1_BADTAG, os error -2146881269>)
```

The same class, same fixture and same runner passes on Linux, and passes on
real HotSpot JDK 25 on Windows.

## Root cause

The test's fixture keystore
(`module/spring-boot-ldap/src/test/resources/org/springframework/boot/ldap/autoconfigure/embedded/test.jks`)
holds a **1024-bit DSA key** with a `SHA1withDSA` self-signed certificate
(`keytool -list -v`: "Signature algorithm name: SHA1withDSA (weak)", "Subject
Public Key Algorithm: 1024-bit DSA key (weak)"). The test starts a real LDAPS
listener from that identity and then completes a real handshake against it
(`server.getConnection("LDAPS").getSSLSession()`), so a working DSA TLS path is
genuinely required on both ends.

rustls cannot provide one. It has no DSA `SigningKey` at all, and its TLS 1.2
cipher suites are all `ECDHE_RSA` / `ECDHE_ECDSA`; a DSA certificate needs a
`DHE_DSS` suite, and the key-exchange suite is selected from the certificate's
key type — so a DSA identity cannot be expressed without adding new cipher
suites to the (already vendored) rustls fork. `rustls::sign::any_supported_type`
therefore reports the generic "failed to parse private key as RSA, ECDSA, or
EdDSA", which is the first half of the message above.

`../../../native-builtins/src/t27_tls.rs` then falls back, per platform:

- `#[cfg(unix)]` → `legacy_dsa_acceptor`, an OpenSSL acceptor with
  `set_security_level(0)`, which accepts DSA and other legacy identities
  (matching client paths exist in `servlet.rs`, `phases_late/ssl_security.rs`
  and `net_phase_e.rs`);
- `#[cfg(not(unix))]` → `native_tls::Identity::from_pkcs8`, i.e. Windows
  SChannel, which rejects the DSA key blob outright (`CRYPT_E_ASN1_BADTAG`) —
  and would not help even if the import succeeded, because modern Windows
  SChannel no longer offers `TLS_DHE_DSS_*` suites.

This is a Unix/Windows platform-parity gap, not a newly-hit rustls limitation:
rustls's DSA gap is long known and already has a working Unix fallback.

## Why it is accepted rather than fixed

Closing it means giving the Windows branch an OpenSSL-backed acceptor, i.e.
putting OpenSSL into the Windows dependency graph. `openssl` is currently
declared only under `[target.'cfg(unix)'.dependencies]` in
`../../../native-builtins/Cargo.toml`, deliberately — "Only the legacy DSA server
fallback needs OpenSSL's per-context security policy API. Keeping it
Unix-scoped preserves the Windows dependency graph."

A feasibility build of `openssl = { version = "0.10", features = ["vendored"] }`
was attempted on a Windows development host and **failed**: `openssl-src` runs
OpenSSL's Perl `Configure`, and Git for Windows' Cygwin perl lacks
`Locale::Maketext::Simple`. Making this work would require a full Perl
distribution on every Windows build machine and CI, plus an OpenSSL
build-from-source on every clean build — a cross-cutting toolchain cost paid by
everyone, to support a legacy 1024-bit DSA key in one test fixture.

Other routes were considered and ruled out:

- **SChannel / `native-tls`** — no `DHE_DSS` cipher suites on modern Windows.
- **A custom rustls `SigningKey`** — rustls has no DSA signature scheme, and a
  DSA certificate cannot select any suite rustls implements (see above).
- **Substituting a different key** — would stop testing the fixture's identity.

This matches the posture already taken for the other legacy-crypto gaps in the
rustls backend (`rustls-cbc-cipher-suites-not-supported.md`,
`rustls-tls11-protocol-not-supported.md`), with the added note that on Unix the
gap *is* covered by the OpenSSL fallback.

## If this is revisited

The code change itself is small and well understood: un-gate
`legacy_dsa_acceptor`, `TlsServerConfig::LegacyDsa`, `TlsServerStream::LegacyDsa`
(`t27_tls.rs`), `s2_legacy_dsa_tls_connect` and `TlsClientStream::LegacyDsa`
(`servlet.rs`), the DSA detection helpers `is_dsa_private_key_pem` /
`is_dsa_certificate_der`, and their two client call sites
(`phases_late/ssl_security.rs`, `net_phase_e.rs`). The blocker is entirely the
Windows OpenSSL toolchain requirement, not the VM code.
