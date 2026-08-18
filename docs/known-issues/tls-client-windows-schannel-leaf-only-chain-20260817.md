# On Windows the `SSLSocket` client still captures only the LEAF certificate

## Status
**OPEN, 2026-08-17.** Narrow remainder of a defect closed on Unix the same day.
NOT MEASURED on Windows — asserted from the code, in the conservative
direction (this page claims the bug is still there, not that it is gone).

## What

`servlet::s2_tls_connect_on`, the `native_tls`-backed client bridge, captures
the peer's leaf certificate and nothing else: `TlsStream::peer_certificate()`
is the only accessor native-tls 0.2 has, and there is no chain accessor on any
of its backends.

On Unix that path is no longer the default — `s2_openssl_tls_connect_on` is,
and it asks OpenSSL for the whole chain. On Windows the `#[cfg(not(unix))]` arm
still selects native-tls, i.e. SChannel, and is byte-for-byte what it was.

The consequence is the one the Unix page measured: an application that installs
its own `TrustManager`s — `SSLContext.init(null, tms, null)`, which is what
HttpClient5's `SSLContextBuilder`, Apache HC and any JDBC driver with a custom
trust store do — is handed a one-certificate chain and cannot build a path to
any root. MEASURED on Linux before the fix, across 20 live public sites: 20
rejections, every one at `chainLen=1`, against 20 acceptances at 2–4 on HotSpot.
`SSLSession.getPeerCertificates()` is the same divergence in its own right, for
certificate pinning or chain inspection.

## Why it was not fixed with the rest

`openssl` is a **deliberately Unix-scoped** dependency of `native-builtins`:

```toml
[target.'cfg(unix)'.dependencies]
# Only the legacy DSA server fallback needs OpenSSL's per-context security
# policy API. Keeping it Unix-scoped preserves the Windows dependency graph.
openssl = "0.10"
```

So the fix that closed this on Unix — moving the connector to a raw
`openssl::SslConnector` — is not available here as written, and no amount of
native-tls *configuration* helps: the accessor does not exist.

## What a fix would need

One of, in rough order of appetite:

1. **rustls for the Windows client path.** `rustls` is already an unconditional
   dependency of this crate and `t27_tls::rustls_client_connect` already exists;
   rustls exposes the full peer chain, which is why the `SSLEngine` path never
   had this defect (`EngineChainProbe`, measured 3/3/4 matching HotSpot). The
   cost is that rustls is stricter than both HotSpot and OpenSSL in ways this
   VM's fixtures currently depend on — its `ring` provider has no RSA below
   2048 bits, and it requires SANs with no CN fallback — so the security-level
   band that the sibling page closes would REOPEN in the other direction, and
   several self-signed fixtures would need checking.
2. **`openssl` with the `vendored` feature on Windows.** Uniform with Unix, and
   the change is then a `#[cfg]` deletion. Costs a perl+nasm build dependency
   and a much heavier Windows dependency graph — the thing the Cargo.toml
   comment above deliberately avoids.
3. **The `schannel` crate directly.** It exposes a certificate context and
   chain-building API, but native-tls does not hand out its inner stream, so
   this means writing a third client bridge.

## How to measure it

Build `cratonvm` on Windows and run, against a live public host:

```
probes/CustomTmProbe.java      # peerChainLen, default factory vs custom-TM context
probes/RealChainProbe.java     # 20 sites, chain handed to the VM's own TrustManager
```

HotSpot answers 3/3/4 and 20 accepts. A `peerChainLen=1` confirms this page; a
2–4 retires it. Nothing else needs to be built — both probes are plain JDK
classes.

## Related
* `tls-client-captures-only-the-leaf-so-a-custom-trustmanager-cannot-validate-20260816-FIXED-20260817.md`
  under `docs/internal/fixed-suite-bugs` — the Unix fix, its measurements, and
  the two further defects it uncovered.
