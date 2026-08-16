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
| Default anchors with NO property set | 118 (cacerts) | **122 (OS store)** | **118** |
| …anchors CratonVM trusted that the JDK does not | — | **4** | **0** |

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

### 1b. And with no property set, the default anchors were the OS store, not `cacerts`

Found by pulling on the "122 vs 118" in the numbers above rather than
accepting it as noise. JSSE's default trust store is
`$JAVA_HOME/lib/security/cacerts` (after `jssecacerts`);
`build_trust_manager_state(0)` builds its set from `rustls-native-certs`, i.e.
the OS store — a different set of CAs.

MEASURED (`TrustSetProbe`), keyed on the SHA-256 of each encoded certificate,
NOT on the subject DN — the two VMs render the same DN differently
(hex-escaped OIDs vs `EMAILADDRESS=`/`SERIALNUMBER=` keywords), and a
subject-keyed diff reported 5 and 9 one-sided anchors that were mostly the
same certificates written twice:

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

Same widening family as the ignored property, and the more consequential half
of it: a certificate issued by any of those four would have been accepted by
CratonVM and refused by the JDK.

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

* `tls::resolve_default_trust_store` implements JSSE's search in its own
  order: `javax.net.ssl.trustStore`, else `<java.home>/lib/security/jssecacerts`,
  else `<java.home>/lib/security/cacerts`. It loads through the same
  `keystore::load_keystore` the explicit-`KeyStore` path already uses (an empty
  password skips the JKS integrity MAC, which is what real-JDK
  `JavaKeyStore` does for a null password and how a trust store is normally
  read), caches by (path, password), and answers 0 — today's platform roots —
  whenever the file is absent, unreadable, unparseable or parses to zero
  entries. A VM with no JDK image (synthetic-jdk mode) finds neither file and
  keeps exactly today's behaviour. Wired into
  `TrustManagerFactory.init(KeyStore)`'s null branch.

* **Two resolvers, deliberately.** `explicit_trust_store_keystore_id` honours
  the property ONLY and never `cacerts`, and it is what the client connect
  path below uses. The distinction is load-bearing: that path stands native
  verification down for the store it resolves, which is the right trade for a
  store someone deliberately configured and the wrong one for `cacerts` —
  applying it there would move every default HTTPS client in the VM off
  OpenSSL's path builder and onto `x509_manager::validate_chain`, a far larger
  change than this. For the same reason `cacerts` is registered for
  `getTrustManagers()` but NOT staged via `set_pending_tm_trust_roots`:
  staging ~118 anchors into `extra_root_ders` makes the connector union them
  with the platform set instead of replacing it, and `legacy_dsa_context`
  scans that slice for a DSA key — so one DSA root in the JDK's own store
  could divert unrelated connections onto the legacy OpenSSL path at security
  level 0.

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
TrustSetProbe default anchors           118          122           118
  … of which the JDK does not trust     —              4             0
TmProbe   default anchors / verdict     118 / REJECT 122 / REJECT  118 / REJECT
TmProbe2  property anchors / verdict    1 / ACCEPT   122 / REJECT  1 / ACCEPT
TlsProbe3 cert IS in the trust store    OK 197ms     REJECTED      OK 53ms
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

## Residue (why this page stays open) — now WITNESSED, and blocked

With NO trust store configured, the client still verifies through OpenSSL at
security level 2. That is stricter than the JDK in a narrow band: an RSA key of
1024–2047 bits, or a SHA-1 signature, on a chain to a PLATFORM root.

This page previously called that unwitnessable, because it needs a chain to a
cacerts anchor and no public CA will issue one. It is witnessable — by making
the anchor. `WeakChainProbe`, against an `openssl s_server` presenting a
1024-bit RSA leaf signed by a 1024-bit CA, with a hard-linked copy of the JDK
image whose `cacerts` trusts that CA and NO `javax.net.ssl.trustStore` set:

```
HOTSPOT  (java.home = the modified image)  HANDSHAKE-OK  328 ms
CRATONVM (--java-home the same image)      REFUSED        60 ms
  SSLHandshakeException: … certificate verify failed … (EE certificate key too weak)
```

Same server, same JDK image, same probe. The JDK's floor is 1024 bits;
OpenSSL's level 2 requires 2048. So this is a measured defect, not a claim.

**It is also blocked**, on
[`tls-client-captures-only-the-leaf-so-a-custom-trustmanager-cannot-validate-20260816.md`](tls-client-captures-only-the-leaf-so-a-custom-trustmanager-cannot-validate-20260816.md).
The obvious fix — move this path onto the VM's own validator, which uses the
JDK's rules rather than OpenSSL's levels — cannot be done first: the client
captures only the leaf certificate, and MEASURED, that validator then rejects
**20 of 20** live public sites with `no trust anchor found for chain`. The
chain has to be fixed before the verifier can move.

Both needs land on the same change: the default client connector moved off
`native_tls::TlsConnector` (which exposes neither `peer_cert_chain()` nor
`set_security_level`) onto a raw `openssl::SslConnector`. That swap has to
re-implement what native-tls does on the VM's busiest client path — SNI, ALPN,
the protocol range, hostname verification — so it wants its own branch and its
own netty/Spring gate runs.

Of the two, the leaf-only chain is much the more urgent: it breaks ordinary
HTTPS for any client that configures a TrustManager. The security-level band
is close to empty in practice, since public CAs stopped issuing 1024-bit RSA
and SHA-1 certificates years ago.

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
