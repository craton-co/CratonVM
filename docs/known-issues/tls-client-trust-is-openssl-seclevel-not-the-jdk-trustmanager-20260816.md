# A default-`SSLContext` client applies OpenSSL's SECLEVEL where the JDK applies its own trust rules

## Status
**MOSTLY FIXED 2026-08-16 — one narrow residue keeps this page open.**

Filed the same day while closing the H2 `TestTools` TLS residual. What was
found on the way turned out to be two defects, not one, and the more serious
of the two was in the opposite direction to the reported symptom.

| | HotSpot 25.0.3 | CratonVM before | CratonVM after |
|---|---|---|---|
| `TrustManagerFactory.init(null)` with `javax.net.ssl.trustStore` set: anchors | 1 | **122** | **1** |
| …and `checkServerTrusted` on the certificate it names | ACCEPTED | **REJECTED** | **ACCEPTED** |
| Client TLS to a peer whose cert is in that trust store | handshake OK | **refused** | **handshake OK** |
| Client TLS with a trust store that does NOT name that cert | refused | refused | refused |
| Client TLS with no trust store configured at all | refused | refused | refused |

**Still open:** with no `javax.net.ssl.trustStore` configured, the client path
keeps OpenSSL as its verifier at security level 2, which is stricter than the
JDK — see "Residue" below.

## What was wrong

### 1. The default trust store ignored `javax.net.ssl.trustStore` (widening)

`TrustManagerFactory.init(null)` means "use the default trust material". In
JSSE that is the platform roots ONLY while the property is unset; once set, it
names the ONLY trust store and REPLACES cacerts. CratonVM recorded keystore id
0 for every null KeyStore, so the property was never read. MEASURED
(`TmProbe2`, a one-certificate store):

```
HOTSPOT  acceptedIssuers=1    checkServerTrusted(that cert) = ACCEPTED
CRATONVM acceptedIssuers=122  checkServerTrusted(that cert) = REJECTED
```

An application that pinned its trust to one private CA was silently given the
whole public root set instead — and still had the one certificate it asked to
trust rejected. Both halves wrong, and the first half in the dangerous
direction.

### 2. The client socket path never consulted a TrustManager at all

`SSLSocketFactory.getDefault()` returns a factory with no `SSLContext` behind
it, so `p68_factory_java_tm_key` answers `None`, native verification stays on,
and OpenSSL decides. OpenSSL applies its SECURITY LEVEL (2 by default:
RSA < 2048 refused) to the peer certificate whether or not the application
trusts it. The JDK's equivalent, `jdk.certpath.disabledAlgorithms`, draws the
line at 1024 bits and exempts a trust anchor from the signature-algorithm
check entirely. So a 1024-bit MD5-self-signed certificate the application has
explicitly installed as its anchor is something JSSE accepts and OpenSSL, at
that level, cannot be told to.

## The fix

**Enabling fact, measured first** (`TmProbe`): CratonVM's own trust evaluation
already agrees with HotSpot on both arms — it rejects an untrusted self-signed
certificate and accepts that same certificate once it is installed as an
anchor, MD5 signature and 1024-bit key notwithstanding. The verdict did not
need to change; only which verifier gets to give it.

* `tls::default_trust_store_keystore_id` resolves `javax.net.ssl.trustStore`
  (+ `…Password`) through the same `keystore::load_keystore` the explicit
  `KeyStore` path already uses, caches it by (path, password), and answers 0 —
  today's platform roots — when the property is absent, unreadable, unparseable
  or `NONE`. Wired into `TrustManagerFactory.init(KeyStore)`'s null branch.

  Note which copy: `init(KeyStore)` is registered TWICE for this class and
  `phases_late::ssl_security`'s copy wins. The first attempt at this fix went
  into `tls.rs`'s shadowed twin and changed nothing observable. Both now call
  the one resolver.

* `new13_connect_and_handshake_on` resolves the same store for a connection
  whose own `SSLContext` configured no trust material, hands those anchors to
  the connector with `disable_built_in_roots(true)` (JSSE's replace, not add),
  and — because that alone still leaves OpenSSL's security level in charge —
  stands native verification down for that case and validates the captured
  chain with `x509_manager::validate_chain`, failing closed. Exactly the shape
  the Java-TrustManager path beside it already uses.

  Scope is deliberately the connections that are MISCONFIGURED today: an
  application that named a trust store and was being validated against the
  platform roots regardless. A connection with no property keeps OpenSSL as
  its verifier, untouched. The anchors are also NOT merged into
  `extra_root_ders`, so a DSA root sitting in someone's trust store cannot
  divert an unrelated connection onto the legacy OpenSSL path (which runs at
  security level 0).

## Verified

Azure host, `--java-home /data/toolchain/jdk-25 --nojit --Xmx 1g`, branch off
`origin/dev` `c69ad84d9`, against a pristine build of the same commit.

```
                                        HotSpot      before        after
TmProbe2  anchors / verdict             1 / ACCEPT   122 / REJECT  1 / ACCEPT
TlsProbe3 cert IS in the trust store    OK 197ms     REJECTED      OK 57ms
TlsProbe4 cert is NOT (negative ctrl)   REJECTED     REJECTED      REJECTED
TlsProbe  no trust store at all         REJECTED     REJECTED      REJECTED
```

`TlsProbe4` is the one that matters for whether this is a fix or a hole: it
installs a trust store holding an unrelated platform root and connects anyway.
Both VMs refuse, and CratonVM now says `PKIX path validation failed: no trust
anchor found for chain` where it used to say `EE certificate key too weak`.

Regression cover: `RJdkSecurity` gains a `defaultTrustStoreProperty` stage —
one-anchor store, its own certificate, an unrelated one — diffed against
HotSpot. It prints no certificate, subject or platform anchor count, because
the two VMs legitimately ship different root sets (118 vs 122).

## Residue (why this page stays open)

With NO trust store configured, the client still verifies through OpenSSL at
security level 2. That is stricter than the JDK in a narrow band: an RSA key of
1024–2047 bits, or a SHA-1 signature, on a chain to a PLATFORM root — accepted
by HotSpot, refused here. It needs the default client connector moved off
`native_tls::TlsConnector` onto a raw `openssl::SslConnector` (the shape
`servlet::s2_legacy_dsa_tls_connect_on` already uses) so
`set_security_level(1)` — the JDK-equivalent threshold — can be set. native-tls
0.2 exposes no security-level control, and nothing short of that connector
swap reaches it.

That swap has to re-implement what native-tls does on the VM's busiest client
path — SNI, ALPN, the protocol range, hostname verification, peer-chain
capture — so it wants its own branch and its own netty/Spring gate runs. It is
not urgent: public CAs stopped issuing 1024-bit RSA and SHA-1 certificates
years ago, so the band is close to empty in practice.

## Repro
```bash
cd apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
$JDK25/bin/java -cp "$CP:<probe-dir>" TlsProbe3 9611          # TRUSTED-HANDSHAKE-OK
<cratonvm-bin> --java-home $JDK25 --nojit -c "$CP:<probe-dir>" TlsProbe3 9621
<cratonvm-bin> --java-home $JDK25 --nojit -c "$CP:<probe-dir>" TlsProbe4 9721   # must REJECT
```

## Related
* The retired
  `bug-h2-suite-fail-cluster-pgserver-tools-memoryunmapper-filelock-timer-20260807`
  write-up — where this was found, and whose "both VMs reject the same
  certificate; only the report differs" claim this corrects.
* `docs/known-issues/netty/openssl-key-material-and-engine-residuals-20260813.md`
  — the SSLEngine half of "let the TrustManager decide", moving the verdict
  INSIDE rustls's verifier. Different path, same principle; neither change
  touches the other's code.
