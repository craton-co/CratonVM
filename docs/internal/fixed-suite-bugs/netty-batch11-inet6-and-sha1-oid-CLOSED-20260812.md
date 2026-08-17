# netty batch 11 — closed: two causes fixed, one disproved, four re-scoped

**Status:** ✅ TRIAGE CLOSED (2026-08-12). Every cause on this page now has a
verdict or an owner. Two were fixed here (with a third defect found underneath
one of them), one turned out not to be a CratonVM defect at all, one was closed
on `dev` while this page sat open, and the rest are re-measured and moved to the
records that own them.

Original triage: [investigate-batch-11.md](investigate-batch-11.md), 15 classes
against a stock HotSpot JDK 25 baseline.

## Cause-by-cause outcome

| # | cause | outcome |
|---|---|---|
| 1 | `Inet6Address.getByAddress` folds a v4-mapped address to 4 bytes | ✅ **FIXED** — `NetUtilTest` 13/14 → **14/14**, matches HotSpot |
| 2 | `sha1WithRSAEncryption` unimplemented in the chain verifier | ✅ **FIXED**, and it was hiding a second defect — `SslContextTrustManagerTest` 0/4 → **4/4** |
| 3 | the DNS transport never comes up | ✅ the **hang is gone** and, as of 2026-08-13, so are the 7 residual failures: they were the batch-09 datagram-bind defect → [record](netty-pcap-write-handler-udp-bind-and-tcp-close-FIXED-20260813.md). `DnsAddressResolverGroupTest` 2/2, `SearchDomainTest` 7/7, `DnsNameResolverTest` 195 ok / 21 failed in 66 s |
| 4 | post-quantum KeyPairGenerators + an `X509CertImpl` accessor | ✅ **CLOSED 2026-08-13** → [pkitesting PQC + initVerify](netty/pkitesting-pqc-and-initverify-FIXED-20260813.md). The delta was 18 tests and three defects, and the `X509CertImpl` accessor was none of them: `Signature.initVerify(Certificate)` was passing the CERTIFICATE as the verification key. `CertificateBuilderTest` now matches HotSpot exactly. |
| 5 | TLS handshake/alert family | → [batch 10](tls-batch10-encrypted-keys-and-handshake-gaps-20260812.md), as this page always said |
| 6 | BouncyCastle provider identity | ❌ **NOT A DEFECT** — `BouncyCastleUtilTest` discovers **0 tests on HotSpot too** |
| 7 | `HashedWheelTimerTest` wall-clock assertion | unchanged: the throughput gap meeting a fixed 650 ms bound, not a correctness defect |

Measured with `CratonRunner`, one class per VM, against HotSpot JDK 25 on the
same classpath:

```
                                  HotSpot    CratonVM before   CratonVM after
NetUtilTest                        14/14         13/14            14/14
SslContextTrustManagerTest          4/4           0/4              4/4
BouncyCastleUtilTest              found=0       found=0          found=0
SearchDomainTest                    7/7        HANG → 1/7         1/7
DnsAddressResolverGroupTest         2/2           1/2              1/2
CertificateBuilderTest         39 ok/74      21 ok/74         21 ok/74
```

## Cause 1 — fixed: the fold belongs to the entry point, not to the address

`NetUtilTest.testIpv4MappedIp6GetByName` blew up in netty with
`ArrayIndexOutOfBoundsException: Index 4 out of bounds for length 4` at
`NetUtil.toAddressString`, because CratonVM handed it an object that answered
`instanceof Inet6Address` while carrying four bytes:

| call | HotSpot | CratonVM before | after |
|---|---|---|---|
| `Inet6Address.getByAddress(null, addr16, -1)` | `Inet6Address`, **16 B**, `0:0:0:0:0:ffff:c0a8:1` | `Inet6Address`, **4 B**, `192.168.0.1` | **matches HotSpot** |
| `InetAddress.getByAddress(addr16)` | `Inet4Address`, 4 B | same — correct | unchanged |
| `InetAddress.getByName("::ffff:192.168.0.1")` | `Inet4Address`, 4 B | same — correct | unchanged |

**The original page pointed at the wrong function.** It named
`net_phase_e.rs::alloc_inet_address`, the allocator. But
`Inet6Address.getByAddress(String, byte[], int)` is **not intercepted at all** —
`--dump-native-registry` shows no registration for it, the real JDK bytecode
builds a correct 16-byte object, and the only CratonVM code that touches it
afterwards is the **reader**: `inet_addr_resolve`'s `holder6` branch, which
reads the 16 real octets back out of `Inet6Address$Inet6AddressHolder.ipaddress`
and then ran them through `hotspot_ip_string`. That helper folds v4-mapped to a
dotted quad — correct for `getByName`/`getByAddress(byte[])`, where HotSpot also
hands back an `Inet4Address` — and `inet_addr_address_bytes` then re-parsed four
bytes out of the folded text.

The fix splits the formatter: `ipv6_uncompressed_text` renders HotSpot's eight
minimal-hex groups with **no fold**, `hotspot_ip_string` keeps the fold for the
entry points that want it, and the `holder6` reader uses the former. Regression
cover: `net_phase_e::tests::ipv6_uncompressed_text_never_folds_a_v4_mapped_address`,
which asserts both halves so neither can be "fixed" by removing the other.

## Cause 2 — fixed, and the OID was only half of it

`SslContextTrustManagerTest`, all 4 tests, failed with

```
java.io.IOException: CertificateException: signature-algorithm OID at index 0
    not implemented (oid bytes=[2a, 86, 48, 86, f7, 0d, 01, 01, 05])
```

`1.2.840.113549.1.1.5` — `sha1WithRSAEncryption`. Adding it took **two** tests
to 4/4 and left two still failing, which is the interesting part.

### The OID half

`verify_one_signature` implemented exactly two algorithms. It now covers the
whole PKCS#1 family (SHA-1/256/384/512-with-RSA) and ECDSA with SHA-256/384/512.

**The page's own caution about `Rsa::pkcs1v15_encode` hard-coding the SHA-256
DigestInfo prefix was stale**: the verify path does not go through it. It
delegates to `crypto::signature::verify_rsa_pkcs1_v15_checked`, which has taken
a `DigestAlgorithm` all along — so the RSA family is a four-line dispatch table
plus one `Rsa::verify_pkcs1_v15` wrapper, not an encoder rewrite. Likewise
ECDSA: `verify_with_digest` takes a pre-hashed digest and truncates to the curve
order itself, so SHA-384/512 need only the right hash over the TBS.

SHA-1 is a **deliberate widening**, recorded at the OID constant: chains that
previously failed closed now verify. That is the HotSpot-parity answer — every
JDK ships `SHA1withRSA` and HotSpot validates these test CAs — and a stricter
policy that only CratonVM enforces reads to an application as "this VM cannot do
TLS". SHA-224 (both the RSA and ECDSA spellings) is named explicitly as
known-and-unimplemented rather than left to the catch-all.

### The half underneath: the exception CLASS was wrong

With the OIDs in, two tests still failed — the two whose expectations are
**mixed** (`testUsingCAsOneAandB`, `testUsingCAsOneAandTwo` each expect one
certificate to be rejected). The two all-positive tests passed. That asymmetry
is the tell: the *verdict* was right and the *exception type* was not.

`X509TrustManager.checkServerTrusted` declares `throws CertificateException`,
and the test rejects-path is

```java
try { tm.checkServerTrusted(new X509Certificate[]{ eecCert }, "RSA"); … }
catch (CertificateException e) { … }
```

CratonVM raised `RuntimeError::IOException` with a message that merely *named*
`CertificateException` — `cert_exception` did it in `x509_manager.rs`, and
`tls.rs` had its own copy with a comment calling it "mapped to IOException at
the native boundary". An `IOException` matches no `catch (CertificateException)`
anywhere, so the intended rejection escaped as an unrelated failure. Both sites
now construct a real `java.security.cert.CertificateException` (and a real
`CertPathValidatorException` on the `PKIXValidator.engineValidate` path), with
the historic `IOException` kept only as the can't-construct fallback so no
configuration can lose a validation failure into a silently-trusted connection.

Same species as the `CyclicBarrier` `BrokenBarrierException` fix and the
`Inflater` `DataFormatException` fix: **a declared checked exception is part of
the contract, and callers discriminate on the type.** A message that names the
class is not the class.

## Cause 3 — the hang is gone; what is left belongs to batch 09

`SearchDomainTest` no longer hangs (the `DatagramChannel.localAddress()` bridge
landed with batch 04). It now reports 7 found / 1 ok / 6 failed, and all six are
one shape:

```
NullPointerException: … the return value of "io.netty.util.concurrent.Future.getNow()" is null
Failed to resolve 'unknown.hostname', couldn't setup transport: [id: 0xf39d2026]
```

i.e. the resolver's datagram transport still never comes up — the batch-09
`NioDatagramChannel.bind()` defect this page already identified, which owns the
~50-line repro. `DnsAddressResolverGroupTest`'s remaining 1 failure is the same.
Nothing further to investigate from this page; the two DNS classes are witnesses
of that record, not defects of their own.

The page's note about **two `DatagramChannel` registrars with different slot
layouts** (`phases_late/net_channels.rs`, deliberately unwired, vs the live one
in `native-io/src/lib.rs`) is unchanged and still the trap to respect before
extending either.

## Cause 4 — the generators landed; the certificates did not

`KeyPairGenerator.getInstance` now succeeds for **ML-DSA, ML-KEM and SLH-DSA**
on CratonVM — SLH-DSA is available here and *not* on HotSpot JDK 25, so
CratonVM is ahead on that one. The page's "9 × ML-DSA / 3 × ML-KEM not
available" is stale.

`CertificateBuilderTest` is nevertheless still 21 ok/74 against HotSpot's 39,
and the per-test diff (CratonVM-only failures, HotSpot's own 35 subtracted) is
**15 tests**: nine ML-DSA/ML-KEM *certificate* cases, five
`createCertIssuedByDifferentAlgorithm*`, and `authenticatingCrossSignedCertificate`,
with `NoSuchMethodError: sun.security.x509.X509CertImpl.getAlgorithm()` visible
in the log. That accessor **does not exist on JDK 25's `X509CertImpl`** (checked
with `javap`), so it is a receiver-confusion, not a missing method — filed with
the measurement in
[pkitesting PQC + initVerify](netty/pkitesting-pqc-and-initverify-FIXED-20260813.md) — **CLOSED 2026-08-13**.

## Cause 6 — not a defect

`BouncyCastleUtilTest` reports `found=0 started=0` on **HotSpot as well**. Its
tests are `@EnabledIf`-gated on a BouncyCastle provider that is not on this
classpath, so nothing runs on either VM. The original observation (a provider
identity mismatch) predates the classpath the suite now uses.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.util.NetUtilTest
# 14/14 expected; add io.netty.handler.ssl.SslContextTrustManagerTest for 4/4.
```

`B11Probe.java` (~90 lines, no netty) prints the cause-1 rows and the
KeyPairGenerator availability table for whichever VM runs it; compare against
`java`.
