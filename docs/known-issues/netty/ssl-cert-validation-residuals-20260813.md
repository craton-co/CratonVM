# `handler.ssl`/OCSP certificate-validation residuals — genuine CratonVM-specific FAILs

**Status:** OPEN, not root-caused, no shared cause claimed (2026-08-13).
Found on Windows, commit `ae2e1d9c8`, isolated (`--shards 1`, `--timeout
180`), same classpath both VMs. This is a raw evidence dump across several
classes that share a *theme* (certificate/trust-manager/TLS-provider
behavior) — no claim that they share one root cause; do not assume a single
fix clears all of them.

## Data

| class | CratonVM | HotSpot | note |
|---|---|---|---|
| `handler.ssl.SslContextBuilderTest` | FAIL 9 ok / 9 fail / 3 aborted (21 found) | **PASS 21/21** | see "environment shift" below |
| `handler.ssl.SslHandlerTest` | FAIL 30 ok / 22 fail (54 found) | ABORTED 53 ok / 1 aborted | signature: `DecoderException: SSLHandshakeException: TrustManager rejected the peer certificate chain: CertificateException` |
| `handler.ssl.ocsp.OcspClientTest` | FAIL 4 ok / 2 fail | PASS 6/6 | |
| `handler.ssl.ocsp.OcspServerCertificateValidatorTest` | FAIL 0 ok / 1 fail | PASS 1/1 | signature: `OCSPException: Error setting up certificate path validation` |
| `handler.ssl.OpenSslKeyMaterialManagerTest` | FAIL 0 ok / 1 fail | PASS 1/1 | OpenSSL-native-backed; HotSpot passing confirms the native lib is present in this environment |
| `handler.ssl.PemEncodedTest` | FAIL 1 ok / 2 fail (3 found) | ABORTED 1 ok / 2 aborted | CratonVM actively fails what HotSpot merely skips via assumption |
| `handler.ssl.CloseNotifyTest` | ABORTED 2 ok / 2 aborted | **PASS 4/4** | see "environment shift" below |

## Environment shift worth noting before assigning blame

`SslContextBuilderTest` and `CloseNotifyTest` both show HotSpot now passing
*more* tests than an earlier internal record measured for the same classes.
`docs/internal/fixed-suite-bugs/netty-tls-batch10-residuals-FIXED-20260813.md`
recorded `SslContextBuilderTest` as `9 ok / 9 f` on HotSpot too (attributed
to netty-tcnative being absent on that host) and `CloseNotifyTest` as
`2 ok / 2 a` on HotSpot. This run's HotSpot got **21/21** and **4/4** clean
on the identical two classes — meaning whatever native library/classpath
gap HotSpot had before is no longer present in this environment. That
changes the read on CratonVM's numbers: `SslContextBuilderTest`'s 9/21 was
previously *dismissed* as "matches a crippled HotSpot," but with HotSpot now
fully capable, CratonVM's 9/21 is a real, uncontested gap — not
environment-explained anymore. Same logic for `CloseNotifyTest`'s 2/4.
Worth checking what changed in the environment (see
`bouncycastleutiltest-no-longer-a-non-defect-20260813.md`, which independently
noticed the same kind of shift for a different class) before assuming these
are new regressions rather than newly-exposed pre-existing gaps.

## Not yet done

No stack traces beyond the `sig` column captured for most of these; no
attempt to find a shared cause across the OCSP/BouncyCastle-adjacent
classes (`OcspClientTest`, `OcspServerCertificateValidatorTest`,
`BouncyCastleUtilTest` in the sibling doc) even though they're plausibly
related given BouncyCastle's own presence-on-classpath shift. HotSpot
cross-check is done for all rows; CratonVM raw logs were not individually
read past the `sig` column extracted by the harness.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.handler.ssl.SslContextBuilderTest io.netty.handler.ssl.SslHandlerTest io.netty.handler.ssl.ocsp.OcspClientTest io.netty.handler.ssl.ocsp.OcspServerCertificateValidatorTest io.netty.handler.ssl.OpenSslKeyMaterialManagerTest io.netty.handler.ssl.PemEncodedTest io.netty.handler.ssl.CloseNotifyTest > /tmp/ssl-residuals.txt
CV_BIN=bin/cratonvm-netty-zgc.exe bash run-netty-suite.sh --list /tmp/ssl-residuals.txt --gc zgc --shards 1 --timeout 180 --out /tmp/repro
bash run-netty-suite.sh --list /tmp/ssl-residuals.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

## Related

- `docs/known-issues/netty/bouncycastleutiltest-no-longer-a-non-defect-20260813.md`
  — same-day, same-shape environment-premise shift for a different class.
- `docs/known-issues/netty/ssl-suite-test-discovery-undercounts-20260813.md`
  — a structurally different problem (discovery-time undercounting) found
  in the same triage pass, do not conflate the two.
- `docs/internal/fixed-suite-bugs/netty-tls-batch10-provider-routing-and-close-notify-FIXED-20260813.md`,
  `docs/internal/fixed-suite-bugs/netty-tls-batch10-residuals-FIXED-20260813.md`
  — prior TLS work; several of these classes appear there with different
  (older, environment-different) baselines.
