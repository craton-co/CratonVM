# netty batch 11 — `Inet6Address.getByAddress`, an unimplemented signature OID, and a dead DNS transport

**Status:** OPEN (2026-08-12). Triage record for
[investigate-batch-11.md](investigate-batch-11.md) — all 15 classes measured
against a stock HotSpot JDK 25 baseline. Nothing here is fixed; two of the
seven causes below are isolated down to a three-line repro and should be quick.

## Subtract the environment first

Four classes are not CratonVM defects at all — `AbstractReferenceCountedTest`,
`DefaultHostsFileEntriesResolverTest` and `InetSocketAddressResolverTest` pass
on both VMs, and `resolver.dns.NativeImageHandlerMetadataTest` fails on both
(the `null/null` Maven-property harness gap batch 08 documented).

Two more have most of their failures on **both** VMs, and counting the raw
totals would have overstated CratonVM's delta by 39 tests:

| class | HotSpot | CratonVM | shared cause | real delta |
|---|---|---|---|---|
| `CertificateBuilderTest` | 39 ok / 28 failed | 21 ok / 46 failed | **24** × `SLH-DSA KeyPairGenerator not available` | 18 |
| `SslHandlerTest` | 37 ok / 15 failed | 27 ok / 25 failed | **15** × `UnsatisfiedLinkError` (netty-tcnative absent) | 10 |

`DnsNameResolverTest` also hits the harness cap on **HotSpot** (rc=124 after
reporting 216 ok / 0 failed), so its "HANG" is partly a fixture that does not
exit cleanly.

## Cause 1 — `Inet6Address.getByAddress` folds an IPv4-mapped address to 4 bytes

`NetUtilTest.testIpv4MappedIp6GetByName`:

```
java.lang.ArrayIndexOutOfBoundsException: Index 4 out of bounds for length 4
    at io.netty.util.NetUtil.toAddressString(NetUtil.java:992)
```

Isolated to three lines. Feeding the 16-byte `::ffff:192.168.0.1` form to
both factories:

| call | HotSpot | CratonVM |
|---|---|---|
| `Inet6Address.getByAddress(null, addr16, -1)` | `Inet6Address`, **16 bytes**, `0:0:0:0:0:ffff:c0a8:1` | `Inet6Address`, **4 bytes**, `192.168.0.1` |
| `InetAddress.getByAddress(addr16)` | `Inet4Address`, 4 bytes, `192.168.0.1` | same — correct |

The v4-mapped fold is right for `InetAddress.getByName`/`getByAddress` (both
VMs agree, and a separate probe confirms `getByName("::ffff:192.168.0.1")`
matches exactly). It is **wrong for `Inet6Address`'s own factory**, which the
JDK specifies to keep the 16-byte form. The result is an object that answers
`instanceof Inet6Address` while carrying four bytes — internally inconsistent,
which is why netty indexes past the end.

Where to look: `net_phase_e.rs::alloc_inet_address` applies
`hotspot_ip_string` (the fold) and then picks the concrete class by re-parsing
the *folded text*, so both the class choice and the later `getAddress()` byte
count come from a string that has already lost the IPv6 shape. The fix has to
keep the fold on the paths HotSpot folds and skip it for `Inet6Address`
construction — note the surrounding comments, which record several previous
regressions in exactly this function (Hazelcast's `DefaultAddressPicker`,
Jetty's connector text).

## Cause 2 — `sha1WithRSAEncryption` is not implemented in the chain verifier

`SslContextTrustManagerTest`, all 4 tests:

```
java.io.IOException: CertificateException: signature-algorithm OID at index 0
    not implemented (oid bytes=[2a, 86, 48, 86, f7, 0d, 01, 01, 05])
```

That OID is `1.2.840.113549.1.1.5` — `sha1WithRSAEncryption`.
`x509_manager.rs::verify_one_signature` implements exactly two algorithms,
`sha256WithRSAEncryption` and `ecdsa-with-SHA256`; everything else falls to
`TrustError::NotImplemented`. The gap is narrower than it looks:
`crypto_impl.rs::oid_to_sig_name` **already recognises** SHA1/384/512-with-RSA
and ECDSA-with-SHA384 for `getSigAlgName()`, and `Sha384`/`Sha512` plus the
`sha1` crate are already in the tree — only the verifier is missing them.

Two cautions for whoever picks this up:

* `Rsa::pkcs1v15_encode` hard-codes the SHA-256 DigestInfo prefix, so the RSA
  family needs the encoder parameterised by digest, not a copy per algorithm.
* Implementing SHA-1 makes chains that currently **fail closed** verifiable.
  That is the HotSpot-parity answer here (HotSpot validates these test CAs),
  but it is a deliberate widening and should be recorded as such rather than
  slipped in.

## Cause 3 — the DNS transport never comes up

Three classes, one family, and it points at an already-filed defect.

* `DnsAddressResolverGroupTest` — `UnknownHostException: Failed to resolve
  'netty.io', couldn't setup transport`, with 6 `ClosedChannelException`s in
  the same log.
* `SearchDomainTest` — **hangs** (7 ok in 6 s on HotSpot). The log ends with
  `NoSuchMethodError: java/nio/channels/DatagramChannel.localAddress()` and
  `Thread-2 terminated with error`; the test then waits forever on a resolver
  that no longer has a thread.
* `DnsNameResolverTest` — hangs, though HotSpot also hits the cap.

`couldn't setup transport` + `ClosedChannelException` is the signature of
[`NioDatagramChannel.bind()` throwing `StacklessClosedChannelException`](pcap-write-handler-three-residuals-20260812.md),
filed from batch 09 with a ~50-line repro. **Fixing that one defect should be
tried before anything else on this page** — it plausibly clears three classes
here plus the three UDP pcap tests there.

The `DatagramChannel.localAddress()` `NoSuchMethodError` is a second, distinct
gap, and the area carries a documented trap: `DatagramChannel` has **two**
registrars with **different slot layouts** — one in
`phases_late/net_channels.rs` (deliberately unwired, see its W7-9 comment) and
the live one in `native-io/src/lib.rs`. Adding a method to the wrong one is the
slot-index species, i.e. heap corruption rather than a wrong answer. Unify the
layout before extending either.

## Cause 4 — post-quantum KeyPairGenerators and one `X509CertImpl` accessor

`CertificateBuilderTest`'s CratonVM-only delta:

* 9 × `ML-DSA KeyPairGenerator not available`, 3 × `ML-KEM ...` — present on
  HotSpot (JDK 24+), absent here. `SLH-DSA` (24 failures) is absent on **both**
  and is not ours.
* 3 × `NoSuchMethodError: sun.security.x509.X509CertImpl.getAlgorithm()` — a
  single missing accessor, and the cheapest item on this page.

## Cause 5 — the TLS handshake/alert family (shared with batch 10)

`SslHandlerTest` (~10 beyond tcnative), `OcspClientTest` (2),
`OcspServerCertificateValidatorTest` (1): `expected: <true> but was: <false>`,
`Unexpected type, expected: <javax.net.ssl.SSLException>`, `Unexpected null
value, expected: <io.netty.handler.ssl.SslHandler...>`. Same shape as
[batch 10's clusters 3 and 4](tls-batch10-encrypted-keys-and-handshake-gaps-20260812.md)
— a handshake that reports success but does not deliver the bytes, alerts or
exceptions that follow. Treat as one investigation with those; do not chase it
separately from this page.

`OcspClientTest` also shows `AbstractMethodError:
javax/net/ssl/HttpsURLConnection.getServerCertificates()` — an unimplemented
abstract, independent of the above.

## Cause 6 — BouncyCastle provider identity

`BouncyCastleUtilTest`, both tests: `expected:
org.bouncycastle.jce.provider.BouncyCastleProvider@…` / `Unexpected type,
expected: <org.bouncycastle.jce.provider.Bouncy…>`. The provider object
CratonVM hands back is not the one the test expects.

## Cause 7 — `HashedWheelTimerTest`, one wall-clock assertion

`Timeout + 100000 delay 658 must be 125 < 650`. A fixed wall-clock bound
missed by 8 ms; this is the throughput gap meeting a tight timing assertion,
not a correctness defect. Lowest priority.

## Suggested order

1. **Cause 3's datagram bind** — already filed with a minimal repro, and it may
   clear six classes across two pages.
2. **Cause 1** — three-line repro, no security implications.
3. **Cause 4's `X509CertImpl.getAlgorithm()`** — one accessor.
4. **Cause 2** — clear target, but parameterise the PKCS#1 encoder and record
   the SHA-1 widening deliberately.
5. **Cause 5** — merge with batch 10 rather than duplicating the work.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.util.NetUtilTest
```
