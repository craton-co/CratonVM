# TLS handshake enforcement: 4 classes where CratonVM's handshake diverged from JSSE — FIXED 2026-09-12

Retired from the `known-issues/tomcat` page of the same name (opened 2026-09-12).

## Status
**FIXED / RECLASSIFIED.** The page's two groups had nothing in common:

| group | classes | verdict |
|---|---|---|
| 1 — expected `SSLHandshakeException` never arrives | `TestSSLHostConfigCipher`, `TestSSLHostConfigCompat`, `TestSSLHostConfigProtocol` | **CratonVM regression, FIXED.** A 2026-09-11 registrar shadowed the only setters that apply a client's cipher/protocol restriction |
| 2 — handshake refused when it should succeed | `ocsp.TestOcspEnabled` | **Fixture, not CratonVM.** The OCSP responder servlet could not load BouncyCastle — the stale-`cp.txt` gap |

The page asked for a HotSpot control first. Done, same fixture, JDK 25.0.3:
all four classes `OK` on HotSpot.

## Measured
| class | HotSpot | before (`target-tomcat-20260912`) | after (`cratonvm-tlsdf-v1`) |
|---|---|---|---|
| `TestSSLHostConfigCipher` | OK (12) | 2 failures | **OK (12)** |
| `TestSSLHostConfigProtocol` | OK (12) | 2 failures | **OK (12)** |
| `TestSSLHostConfigCompat` | OK (78) | 4 failures | **OK (78)** |
| `ocsp.TestOcspEnabled` | OK (116)¹ | 5 failures in the 2026-09-12 suite runs; OK (116) ×4 once `cp.txt` resolved | **OK (116)** |

¹ with the BouncyCastle 1.84 jars on the classpath; with the stale `cp.txt`
HotSpot cannot even start the responder (`NoClassDefFoundError:
org/bouncycastle/operator/OperatorCreationException`).

## Group 1 — the restriction setters were shadowed

`CRATONVM_DBG_TLS_AUTH=1` on `TestSSLHostConfigProtocol` showed the SERVER
honouring its connector exactly (`protocols=["TLSv1.2"] -> versions=[TLSv1_2]`)
and the CLIENT probe coming back empty:

```text
huc_client_tls_restrictions: factory class=TesterSupport$ClientSSLSocketFactory placeholder=false
huc_client_tls_restrictions host=localhost -> ciphers=[] protocols=[]
```

`HttpsURLConnection` learns a factory's restrictions by up-calling
`createSocket` in probe mode; Tomcat's `ClientSSLSocketFactory.reconfigureSocket`
then calls `setEnabledCipherSuites` / `setEnabledProtocols` on the probe
socket, and `net_phase_e`'s natives record them (or, on an eager or layered
socket, reconnect / store them for the deferred handshake). Not one
`SSLSocket.setEnabled*` debug line printed: those natives never ran.

`ee264ea33` (2026-09-11, "tls: the client socket kept the AbstractMethodErrors
G25 fixed on the server") added validating `SSLSocket.setEnabledCipherSuites`
/ `setEnabledProtocols` natives in `t27_tls::register_client_socket_mode_accessors`.
`t27_tls` registers after `net_phase_e`, so they won by last-writer-wins —
and they validated the names and returned. Every client-side cipher and
protocol restriction in the VM became a no-op: a TLS-1.3-only client against a
TLS-1.2-only connector negotiated 1.2, a `TLS_DHE_RSA_*`-only client against an
EC-only host negotiated an ECDSA suite. This is the same shape as doc 21's
defect 1b (`21-tls-handshake-enforcement-gap-FIXED.md`),
which closed these very classes in July.

**Fix:** one implementation. `net_phase_e::ssl_socket_set_enabled_cipher_suites`
and `ssl_socket_set_enabled_protocols` now validate first (HotSpot's messages,
moved from `t27_tls`) and then apply the restriction; BOTH registrars name
those functions, so which one wins no longer decides the behaviour.

Not a "config knob too permissive" in the rustls config path, and not the
keystore classpath gap — both hypotheses on the original page were wrong.

## Group 2 — the OCSP responder had no BouncyCastle

The failing run's `.log.err`, at each failing parameterization:

```text
SEVERE [http-nio-8888-exec-1] StandardWrapperValve.invoke ... servlet [responder]
java.lang.NoClassDefFoundError: org/bouncycastle/jce/provider/BouncyCastleProvider
    at org.apache.tomcat.util.net.ocsp.TesterOcspResponderServlet.init(TesterOcspResponderServlet.java:95)
```

Only the cases where the client VERIFIES the server's OCSP status
(`serverOk true, verifyServer true`) need a live responder, which is exactly
the five that failed. `cp.txt` named Gradle-cache jars that no longer exist —
`cp-txt-stale-gradle-module-cache-paths-20260912.md`. With the jars resolvable
(`.m2` copies via `-ExtraCp` for the HotSpot control; `run-one.ps1` has since
started refreshing `cp.txt` itself) the class passes on both VMs, including
five OCSP classes at `-Parallel 2` under the suite runner with
`CRATONVM_C2_SUPERSEDE=0` (`tlsdf-ocsp-repro1`: 5 PASS).

Also checked and ruled out: `FileChannel.lock()` on Windows does exclude
across CratonVM processes and against HotSpot (`tryLock` answers `null`,
`lock()` waits for the holder), so concurrent OCSP classes do serialise.

## Regression check
20 neighbouring TLS / OCSP / date classes, one process each, on the before and
after binaries: **identical in all 20.** 17 `OK` on both; the three that are
not were red on both and for already-recorded reasons —

| class | both binaries | recorded in |
|---|---|---|
| `TestSsl` | 1 failure: `testClientInitiatedRenegotiation[JSSE]` | `ssl-renegotiation-emulation-limits.md` (by design, rustls has no renegotiation) |
| `TestClientCert` | 1 failure: `testClientCertPostZero[JSSE]` (`OK-1024` for `OK-0`) | same page |
| `TestHttp2InitialConnection` | 4 failures: `content-language` `ru` for `en` | `windows-local-environment-artifacts.md` (host OS locale) |

`OK` on both: `TestClientCertTls13` (6), `TestCustomSslTrustManager` (9),
`TestLargeClientHello` (1), `TestSslHandshakeFailure` (1), `TestSSLHostConfig`
(11), `TestSSLHostConfigIntegration` (3), `TestSSLValve` (19),
`TestResolverSSL` (3), `TestSecurity2017Ocsp` (5), `TestOcspSoftFail` (15),
`TestOcspSoftFailInternalError` (20), `TestOcspSoftFailTryLater` (20),
`TestExpiresFilter` (19), `TestCookieProcessorGeneration` (30),
`TestFastHttpDateFormat` (1).

`native-builtins` integration gates `registrar_drift`, `registry_contracts`,
`stub_ratchet`, `eintr_ratchet`, `lock_discipline_ratchet`: all pass.

## Files
- `native-builtins/src/net_phase_e.rs` — shared `ssl_socket_set_enabled_{cipher_suites,protocols}`
- `native-builtins/src/t27_tls.rs` — registers the shared functions

## Repro
```powershell
apps\tomcat-suite-runner\run-one.ps1 -Exe <cratonvm.exe> -Class org.apache.tomcat.util.net.TestSSLHostConfigProtocol
$env:CRATONVM_DBG_TLS_AUTH='1'   # huc_client_tls_restrictions must name the restriction
```
