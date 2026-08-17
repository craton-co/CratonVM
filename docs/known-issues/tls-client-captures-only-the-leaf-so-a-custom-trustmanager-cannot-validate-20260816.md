# An `SSLSocket` client captures only the LEAF certificate, so an application `TrustManager` cannot validate any real chain

## Status
**OPEN, found 2026-08-16.** Differential-verified against Temurin 25.0.3 on the
Azure host, against live public servers. Found while trying to close the
security-level residue in
[`tls-client-trust-is-openssl-seclevel-not-the-jdk-trustmanager-20260816.md`](tls-client-trust-is-openssl-seclevel-not-the-jdk-trustmanager-20260816.md),
which cannot be closed before this is.

An application that installs its own `TrustManager`s — the normal shape for
HttpClient5's `SSLContextBuilder`, Apache HC, a JDBC driver with a custom trust
store, anything that calls `SSLContext.init(null, tms, null)` — **cannot
connect to any ordinary public HTTPS server**, because the VM hands that
TrustManager a one-certificate chain and it cannot build a path to a root.

## Measured

`CustomTmProbe`, same run, same hosts, TrustManagers from
`TrustManagerFactory.init(null)` (i.e. the JDK's own default managers — not
anything exotic):

```
                          HotSpot 25.0.3          CratonVM
github.com
  default factory         OK  peerChainLen=3      OK    peerChainLen=1
  custom-TM context       OK  peerChainLen=3      FAIL  SSLHandshakeException:
                                                        TrustManager rejected the peer
                                                        certificate chain: no trust anchor
www.google.com            (identical shape)
repo.maven.apache.org     OK  peerChainLen=4      OK 1 / FAIL
```

The default factory still connects because OpenSSL verifies internally with
its own chain. The custom-TM context is the one that breaks, and it breaks
because CratonVM correctly stands the native verifier down there — making the
Java TrustManager the only verifier — and then gives it nothing to work with.

`RealChainProbe` puts a number on the same thing across 20 public sites: the
VM's default TrustManager, handed the chain the VM captured, rejects **20 of
20** with `no trust anchor found for chain`, every one at `chainLen=1`. The
same probe on HotSpot: 20 handshakes, 20 accepts, chains of 2–4.

## Root cause, and why the reasoning was once correct

`servlet::s2_tls_connect_on` says so itself:

```rust
// Capture the peer's leaf certificate DER bytes. native-tls's public API
// only exposes the leaf via `peer_certificate()`; the full chain is
// validated internally by the backend (SChannel / SecureTransport /
// OpenSSL) before `connect` returns, which is why we can rely on a
// single-element chain here without weakening security.
```

That was true when written: the backend was the verifier, and the captured
leaf was only ever informational. It stopped being true when the
`java_tm_key` path was added — that path disables native verification
precisely so the Java TrustManager can decide, and it consumes the same
`peer_cert_chain_der` vector. The comment's premise was invalidated by a
later change to a different function, and nothing re-checked it.

## Why no test caught it

Every TLS fixture in the tree uses a **self-signed** certificate — H2's baked
identity, the netty test certs, the regression-suite keystores. For a
self-signed peer the leaf IS the trust anchor, so a one-element chain
validates perfectly. The defect is invisible to the entire corpus by
construction, and only appears against a real CA-issued chain.

That is the generalisable part: a fixture whose certificate is self-signed
cannot exercise chain building at all.

## Scope

* CONFIRMED: `SSLSocket` clients via `SSLSocketFactory`, both the immediate
  `createSocket(host, port)` overloads and the deferred `createSocket()` +
  `connect()` pattern — they share `new13_connect_and_handshake_on`.
* CONFIRMED: `SSLSession.getPeerCertificates()` on those sockets returns 1
  certificate where HotSpot returns 2–4, which is an API divergence in its own
  right for certificate pinning or chain inspection.
* NOT VERIFIED: the `SSLEngine` path (`t27_tls`, rustls-backed) — rustls
  exposes the full chain to its verifier, so netty may well be unaffected.
  Worth measuring before sizing the fix.

## What a fix needs

The chain has to come from the backend, and native-tls 0.2 does not expose it
— `peer_certificate()` is the leaf and there is no chain accessor. The
`openssl` crate does: `SslRef::peer_cert_chain()`.

So this needs the default client connector moved off
`native_tls::TlsConnector` onto a raw `openssl::SslConnector`, which is the
same change the security-level residue needs
(`set_security_level`, also absent from native-tls). One connector swap buys
all three:

1. the full peer chain, so an application TrustManager can validate;
2. `getPeerCertificates()` matching HotSpot;
3. the security level aligned with the JDK's own floor.

`servlet::s2_legacy_dsa_tls_connect_on` is a working in-tree template for the
raw connector, and `TlsClientStream::LegacyDsa` already carries an
`openssl::ssl::SslStream` through read/write/close — but it sets
`set_verify_hostname(false)` and security level 0 for its own legacy reasons,
neither of which may be copied onto the default path.

**Do not switch the verifier before the chain is fixed.** MEASURED: with the
chain as it is today, moving the default path onto
`x509_manager::validate_chain` rejects 20 of 20 real sites.

## Repro
```bash
$JDK25/bin/java -cp <probe-dir> CustomTmProbe                       # OK 3, OK 3
<cratonvm-bin> --java-home $JDK25 --nojit -c <probe-dir> CustomTmProbe   # OK 1, FAIL
```
`CustomTmProbe` connects to three public hosts twice each — once with the
default factory, once with an `SSLContext` initialised with the JDK's own
default TrustManagers — so a difference between the two isolates the chain
rather than the trust store.

## Related
* [`tls-client-trust-is-openssl-seclevel-not-the-jdk-trustmanager-20260816.md`](tls-client-trust-is-openssl-seclevel-not-the-jdk-trustmanager-20260816.md)
  — the residue this blocks, and the same connector swap fixes both.
