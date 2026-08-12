# netty `handler.ssl` — encrypted private keys, a DER mismatch, and handshake gaps

**Status:** OPEN (2026-08-12). Triage record for
[investigate-batch-10.md](investigate-batch-10.md) — all 15 classes measured
against a stock HotSpot JDK 25 baseline. Nothing on this page is fixed yet;
this record exists so the next session starts from measurements rather than
from the raw FAIL list.

## What the baseline changed

Six of the 15 are **not** CratonVM defects:

| class | HotSpot | CratonVM | reading |
|---|---|---|---|
| `BouncyCastleEngineAlpnTest` | 0 ok / 1 failed | 0 ok / 1 failed | fails on both |
| `OpenSslKeyMaterialManagerTest` | 0 ok / 1 failed | 0 ok / 1 failed | fails on both |
| `PemEncodedTest` | 1 ok / 2 failed | 1 ok / 2 failed | fails on both |
| `EnhancedX509ExtendedTrustManagerTest` | 6 ok | 6 ok | passes on both |
| `OptionalSslHandlerTest` | 4 ok | 4 ok | passes on both |
| `PkiTestingTlsTest` | 3 ok | 3 ok | passes on both |

`SslContextBuilderTest`'s **9** HotSpot failures are all
`UnsatisfiedLinkError: failed to load the required native library` —
netty-tcnative/OpenSSL is not present on this host. CratonVM shows 12, so its
own delta is 3, not 12. `SniClientTest`'s HotSpot failure is
`ChannelException: address already in use` — a port collision, not a defect.
**Subtract the environment before counting.**

That leaves the real gaps below.

## Cluster 1 — encrypted private keys: `PBEWithMD5AndDES` is not implemented

The largest cluster, ~12 failures across four classes.

```
java.security.NoSuchAlgorithmException: Cannot find any provider supporting PBEWithMD5AndDES
java.lang.IllegalArgumentException: File does not contain valid private key:
    .../io/netty/handler/ssl/test_encrypted.pem
```

- `SniHandlerTest` — 7 failures, all `test_encrypted.pem`
- `SslContextBuilderTest` — 1 (`Input stream does not contain valid private key`)
- `JdkSslClientContextTest` / `JdkSslServerContextTest` — 2 each, naming the
  algorithm directly

netty's `SslContext.generateKeySpec` reads the PKCS#8 encryption OID, then asks
for `SecretKeyFactory.getInstance(alg)` and `Cipher.getInstance(alg)`. CratonVM
implements **PBES2** (`PBEWithHmacSHA{1,224,256}AndAES_{128,256}`, see
`jca/cipher.rs::pbes2_aes_params`) but not **PBES1** — PKCS#5 v1.5, an MD5-based
KDF over `password || salt` iterated `c` times, split 8/8 into a DES key and IV,
then DES-CBC.

**Do not "fix" this by admitting the name.** `jca/cipher.rs::cipher_family`
carries its own history on exactly that: ChaCha20 and RC4 were admitted while
nothing computed them, which silently served AES-ECB under another algorithm's
name, and were then deliberately refused until implemented. `None` there is a
refusal, not a default. The work is a real PBES1 implementation plus its
`SecretKeyFactory` and `AlgorithmParameters` surfaces; DES/DESede already route
through the real SunJCE SPI, so the cipher half has somewhere to land.

## Cluster 2 — PKCS#1 AES-encrypted keys: `IOException: Invalid lenByte`

~8 failures, 4 each in `JdkSslClientContextTest` / `JdkSslServerContextTest`
(`testPkcs1AesEncryptedRsa` and siblings):

```
java.io.IOException: Invalid lenByte
    at sun.security.util.DerValue.<init>(DerValue.java:411)
    at sun.security.util.DerValue.wrap(DerValue.java:338)
    at javax.crypto.EncryptedPrivateKeyInfo.<init>(EncryptedPrivateKeyInfo.java:92)
    at io.netty.handler.ssl.SslContext.generateKeySpec(SslContext.java:1177)
```

This is **the real JDK's own DER parser**, running on CratonVM, rejecting the
bytes it was handed. HotSpot passes the same test on the same file, so the
bytes differ — the defect is upstream of the parser, in whatever produces them
(the PEM body extraction or the Base64 decode). That makes it a cheap thing to
isolate: dump the byte array netty passes to `EncryptedPrivateKeyInfo` on both
VMs and diff. Do that before theorising about DER.

Distinct from cluster 1 — this file is a traditional OpenSSL PKCS#1 blob
(`Proc-Type: 4,ENCRYPTED` / `DEK-Info: AES-…`), not PKCS#8, so it fails before
any PBE algorithm is looked up.

## Cluster 3 — handshake completes but carries no data

~5 failures: `ApplicationProtocolNegotiationHandlerTest` (2),
`CloseNotifyTest` (2), `SniClientTest` (1 beyond HotSpot's port collision). All
JDK-provider parameterisations; the OpenSSL ones abort for lack of tcnative.

```
java.lang.AssertionError:
Expecting actual:
  0
to be greater than or equal to:
  7
```

The handshake reports success and then zero application bytes arrive where the
test expects at least 7. `CloseNotifyTest` failing in the same shape points at
the same place — data/close-notify delivery through the engine rather than
negotiation. Note batch 08 fixed a TLS defect one layer down (a JDK-generated
EC identity that ring refused); this is a different symptom and needs its own
minimal probe: two channels, one `SslHandler` pair, write N bytes, assert N
arrive.

## Cluster 4 — two classes that do not finish

`ParameterizedSslHandlerTest` and `JdkSslEngineTest` both hit the harness cap
(`rc=124`).

`JdkSslEngineTest` is the one to be careful with: it takes **178 s on HotSpot**
— 821 tests — so its "HANG" in the original Windows run is at least partly the
180 s wall cap, and a slow-but-finishing run would look identical. It has not
been separated.

`ParameterizedSslHandlerTest` is the opposite and **is a confirmed hang**:
**6 s on HotSpot**, and with JUnit's own `@Timeout` disabled
(`-Djunit.jupiter.execution.timeout.mode=disabled`) CratonVM was still running
after 17 minutes — ~170×.

`--stack-dump-on-timeout=90` names the wait site exactly:

```
tid=0 name="main" frames=92
  depth=89  ParameterizedSslHandlerTest.testAlertProducedAndSend(SslProvider,SslProvider)  bci=238
  depth=90  io/netty/util/concurrent/DefaultPromise.syncUninterruptibly()
  depth=91  io/netty/util/concurrent/DefaultPromise.awaitUninterruptibly()
```

The test is `testAlertProducedAndSend` — it drives a handshake that must fail
and asserts the peer both **produces and sends a TLS alert**. Main blocks
forever in `awaitUninterruptibly()` on a promise that is never completed,
i.e. **the alert never arrives**. That is very likely the same underlying gap
as cluster 3: the engine negotiates, but the bytes it should emit afterwards —
application data there, an alert here — never reach the peer. Treat clusters 3
and 4a as one investigation until a probe separates them.

## Suggested order

1. **Cluster 4a + 3 together** — the hang gives the sharpest signal (a named
   wait site and a promise that never completes), and a fix there may take the
   five cluster-3 assertion failures with it. Start from a minimal probe: one
   `SslHandler` pair over a channel pair, force a handshake failure, assert the
   alert is delivered.
2. **Cluster 2** — smallest, and the diff-the-bytes experiment is decisive.
3. **Cluster 1** — biggest raw payoff (12 failures) but a genuine crypto
   feature, so the most work.
4. **Cluster 4b** (`JdkSslEngineTest`) — separate hang from slow first; at
   178 s on HotSpot it may need nothing but a bigger cap.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.SniHandlerTest
```
