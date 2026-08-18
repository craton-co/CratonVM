# A default-`SSLContext` client applies OpenSSL's SECLEVEL where the JDK applies its own trust rules

## Status
**FIXED 2026-08-17** on Unix, on branch `fix/tls-client-chain-and-seclevel-20260817`.
Mostly fixed 2026-08-16; the one residue that kept it open is now closed, by
the connector swap it was blocked behind.

| | HotSpot 25.0.3 | before | after 08-16 | after 08-17 |
|---|---|---|---|---|
| `TrustManagerFactory.init(null)` with `javax.net.ssl.trustStore` set: anchors | 1 | **122** | 1 | 1 |
| …and `checkServerTrusted` on the certificate it names | ACCEPTED | **REJECTED** | ACCEPTED | ACCEPTED |
| Client TLS to a peer whose cert is in that trust store | handshake OK | **refused** | OK | OK |
| Client TLS with a trust store that does NOT name that cert | refused | refused | refused | refused |
| Client TLS with no trust store configured at all | refused | refused | refused | refused |
| `getTrustManagers()` anchors with NO property set | 118 (cacerts) | **122 (OS store)** | 118 | 118 |
| **the CONNECT path's anchors with no property set** | **cacerts** | **OS store** | **OS store** | **cacerts** |
| **1024-bit RSA chain to a platform anchor** | **handshake OK** | **refused** | **refused** | **handshake OK** |

The last two rows are what this update closes. Everything above them was
already fixed on 08-16 and is unchanged; the history is kept below because the
residue is only intelligible against it.

---

## What was wrong (08-16, fixed then)

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

### 1b. And with no property set, the default anchors were the OS store, not `cacerts`

Found by pulling on the "122 vs 118" rather than accepting it as noise. JSSE's
default trust store is `$JAVA_HOME/lib/security/cacerts` (after `jssecacerts`);
`build_trust_manager_state(0)` built its set from `rustls-native-certs`, i.e.
the OS store — a different set of CAs.

MEASURED (`TrustSetProbe`), keyed on the SHA-256 of each encoded certificate,
NOT on the subject DN — the two VMs render the same DN differently
(hex-escaped OIDs vs `EMAILADDRESS=`/`SERIALNUMBER=` keywords), and a
subject-keyed diff reported 5 and 9 one-sided anchors that were mostly the same
certificates written twice:

```
HotSpot cacerts    118 anchors
CratonVM OS store  122 anchors        overlap 118
only in the JDK      0
only in CratonVM     4
```

A strict SUPERSET — nothing the JDK trusts was missing, and four CAs were
trusted here that the JDK does not:

* `CN=Entrust Root Certification Authority` — a root the JDK has already
  distrusted
* `CN=Izenpe.com`
* `CN=SecureSign Root CA12`
* `CN=vm1.…gx.internal.cloudapp.net` — **the build host's own self-signed
  machine certificate**, which lives in `/etc/ssl/certs` and was therefore a
  trusted CA for every default-context client in the VM

### 2. The client socket path never consulted a TrustManager at all

`SSLSocketFactory.getDefault()` returns a factory with no `SSLContext` behind
it, so `p68_factory_java_tm_key` answered `None`, native verification stayed
on, and OpenSSL decided — applying its SECURITY LEVEL (2 by default: RSA < 2048
refused) to the peer certificate whether or not the application trusts it.

### The 08-16 fix

`tls::resolve_default_trust_store` implements JSSE's search in its own order
(`javax.net.ssl.trustStore`, else `jssecacerts`, else `cacerts`), loading
through the same `keystore::load_keystore` the explicit-`KeyStore` path uses
and answering 0 — today's platform roots — whenever the file is absent,
unreadable, unparseable or parses to zero entries. Two resolvers deliberately:
`explicit_trust_store_keystore_id` honours the property ONLY, and is what the
client connect path uses; `default_trust_store_keystore_id` also answers
`cacerts`, and backs `getTrustManagers()`.

Note which copy: `init(KeyStore)` is registered TWICE for this class and
`phases_late::ssl_security`'s copy wins. The first attempt at that fix went
into `tls.rs`'s shadowed twin and changed nothing observable.

---

## The residue, and how it is closed

With NO trust store configured, the client still verified through OpenSSL at
security level 2 — stricter than the JDK in a narrow band: an RSA key of
1024–2047 bits, or a SHA-1 signature, on a chain to a PLATFORM root.

This page once called that unwitnessable, because it needs a chain to a cacerts
anchor and no public CA will issue one. It is witnessable — by making the
anchor. `WeakChainProbe` (`probes/mkweakca-witness.sh` builds the fixture: a
1024-bit RSA CA, a 1024-bit leaf for localhost signed by it, and a hard-linked
copy of the JDK image whose `cacerts` trusts that CA), with NO
`javax.net.ssl.trustStore` set:

```
HOTSPOT   (java.home = the modified image)   HANDSHAKE-OK   321 ms
CRATONVM  before                             REFUSED         63 ms
            SSLHandshakeException: … (EE certificate key too weak)
CRATONVM  after                              HANDSHAKE-OK    53 ms
```

Both arms taken from ONE binary via `CRATONVM_TLS_OPENSSL_CLIENT=0`.

### It needed TWO changes, not one — and the second was the load-bearing half

The obvious reading is "lower the security level". That alone would **not** have
made this probe pass, and finding out why is the part worth recording.

The weak CA exists only in the modified image's `cacerts`. It is not in
`/etc/ssl/certs`. So with the level lowered and the OS store still supplying
the anchors, the handshake would simply have failed one error later — `unable
to get local issuer certificate` instead of `EE certificate key too weak`. A
refusal either way, and a fix that reported itself as working on the level it
had changed.

So the connect path's anchors moved too, from the OS store to the same
`cacerts` that `getTrustManagers()` already used — the row this page had marked
fixed for the trust-manager surface and left open for the connector. That is
the more consequential half in its own right: it is what stops a certificate
issued by any of those four CAs, **including the build host's own machine
certificate**, from being accepted by a default-context client here and refused
by the JDK.

Both are only expressible on the raw `openssl::SslConnector` the chain fix
installed. native-tls has no security-level control, and no way to REPLACE the
built-in roots while keeping its own verifier.

### What did NOT change

The explicitly-configured-trust-store path still stands OpenSSL down and
validates with `x509_manager::validate_chain`, even though the connector can
now set a level. A level is a floor on key sizes and signature algorithms; it
is not the JDK's rule, which exempts a trust ANCHOR from the
signature-algorithm check entirely. MEASURED on the arm that case exists for
(`TmProbe`): the VM's own validator accepts a 1024-bit MD5-self-signed
certificate installed as the anchor and rejects the same certificate when it is
not, matching HotSpot on both. Moving a just-fixed path onto a different
verifier to save one branch would put that verdict at risk for nothing.

The additive per-`SSLContext` custom-anchor case is also unchanged: those roots
still ADD to the platform set, as they always have here.

## The discriminating control

A fix that "works" because the probe cannot fail is not a fix. The same binary,
same server, same probe, against the **unmodified** JDK image — whose `cacerts`
does not hold the weak CA:

```
CRATONVM after, unmodified image   REFUSED  (self-signed certificate in certificate chain)
HOTSPOT        unmodified image    REFUSED  (PKIX path building failed)
```

Both refuse. The acceptance above therefore depended on the modified `cacerts`
being the anchor set, which is exactly the claim.

Regression cover:
`servlet::openssl_client_tests::client_security_level_matches_the_jdks_1024_bit_floor`
serves a 1024-bit chain from an in-process `SslAcceptor` pinned to level 0 (so
only the CLIENT's level is under test) and requires the handshake to succeed;
`client_still_refuses_a_chain_that_reaches_no_configured_anchor` is its paired
refusal. Both were verified by putting the level back to 2 and to leaf-only
capture and watching the right one fail. `RJdkSecurity`'s
`defaultTrustStoreProperty` stage keeps the 08-16 half.

## Repro
```bash
bash probes/mkweakca-witness.sh                      # builds /data/weakca
cd /data/weakca && openssl s_server -accept 9801 -cert leaf.pem -key leaf.key \
    -cert_chain ca.pem -www -cipher 'ALL:@SECLEVEL=0' -ciphersuites TLS_AES_128_GCM_SHA256 &
$WEAKJDK/bin/java -cp <probe-dir> WeakChainProbe 9801                        # HANDSHAKE-OK
<cratonvm-bin> --java-home $WEAKJDK --nojit -c <probe-dir> WeakChainProbe 9801  # HANDSHAKE-OK
CRATONVM_TLS_OPENSSL_CLIENT=0 <cratonvm-bin> … WeakChainProbe 9801           # REFUSED (key too weak)
<cratonvm-bin> --java-home $JDK25 --nojit -c <probe-dir> WeakChainProbe 9801 # REFUSED (control)
```

## What is still open

Windows, for the same reason as the page below: `openssl` is a Unix-scoped
dependency and the SChannel-backed path is unchanged. SChannel has no SECLEVEL,
so the specific band on this page does not exist there; the chain half does —
see `tls-client-windows-schannel-leaf-only-chain-20260817.md` under
`docs/known-issues`.

## Related
* `tls-client-captures-only-the-leaf-so-a-custom-trustmanager-cannot-validate-20260816-FIXED-20260817.md`
  — the blocker; the same connector swap closes both, and carries the full
  measurement table.
* The retired
  `bug-h2-suite-fail-cluster-pgserver-tools-memoryunmapper-filelock-timer-20260807`
  write-up — where this was found, and whose "both VMs reject the same
  certificate; only the report differs" claim this corrects.
* `netty/openssl-key-material-and-engine-residuals-20260813.md` — the SSLEngine
  half of "let the TrustManager decide". Different path, same principle;
  `EngineChainProbe` has since measured that path as unaffected by the chain
  defect.
