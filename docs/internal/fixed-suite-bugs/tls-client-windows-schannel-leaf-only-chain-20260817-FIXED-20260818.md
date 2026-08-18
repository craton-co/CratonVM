# On Windows the `SSLSocket` client still captures only the LEAF certificate

## Status
**FIXED 2026-08-18**, on branch `fix/tls-windows-client-chain-20260818`.
Filed 2026-08-17 as the remainder of a defect closed on Unix the same day, and
filed explicitly as **NOT MEASURED** — asserted from the code, in the
conservative direction. It is measured now, on a Windows box against Temurin
25.0.3, and it was exactly what the page claimed.

```
                            HotSpot 25.0.3     CratonVM Windows
                                                before      after
CustomTmProbe github.com    OK len=3           OK len=1    OK len=3
  … custom-TM context       OK len=3           FAIL        OK len=3
  repo.maven.apache.org     OK len=4           OK len=1    OK len=4
RealChainProbe, 20 sites    20 accept          0 accept    20 accept
  … chain length            2-4                1           2-4
PeerChainOrderProbe         6/6 ordered        —           6/6, CN-for-CN
EngineChainProbe            3/3/4              3/3/4       3/3/4
```

Both CratonVM columns come from ONE binary: `CRATONVM_TLS_OPENSSL_CLIENT=0`
selects native-tls on Windows exactly as it does on Unix, so "before" is a
live arm rather than a separately-built tree.

## The fix

native-tls **is** SChannel on Windows: `imp/schannel.rs` is a ~60-line wrapper
over `SchannelCred` + `tls_stream::Builder`. It exposes neither the inner
stream nor a chain accessor, which was the whole defect — because SChannel
*does* have the chain. The remote certificate's `CertContext` carries an
attached store, documented by Microsoft as "a certificate store containing any
intermediate certificates provided by the remote sender", reachable as
`CertContext::cert_store()`.

So the default client path names the `schannel` crate directly and keeps the
stream in reach (`servlet::s2_schannel_tls_connect_on`).

**This is not a new dependency.** `schannel` is already in `Cargo.lock` at this
version, because native-tls pulls it. The Windows dependency GRAPH is
unchanged; only this crate's view of it is. That matters — the Cargo.toml
comment that scoped `openssl` to Unix exists to protect exactly that graph, and
this respects it rather than working around it. The other two options the
original page listed are both worse for it: `openssl` with `vendored` adds a
perl+nasm build dependency to every Windows build, and rustls would reopen the
security-level band the sibling page closed (its `ring` provider has no RSA
below 2048 bits).

### Deliberately narrower than the Unix connector

There is no `replace_roots` here. On Unix `set_verify_cert_store` genuinely
REPLACES the anchor set, so JSSE's "this store instead of `cacerts`" is
expressible, and the Unix fix used it to stop the OS store from being trusted
in place of the JDK's. SChannel has no such switch: the closest thing
native-tls does for `disable_built_in_roots` is run the platform's normal
verification and then REJECT anything whose built chain does not touch a
configured root. That is a narrowing, never a replacement — it can only refuse
chains the platform accepted. Pointing it at `cacerts` would refuse any site
whose root the Windows store lacks and `cacerts` has, which breaks working
connections in order to fix nothing.

So every trust decision on this path is the decision native-tls was already
making, including both stand-downs. **The chain is the only thing that
changes.** The anchor-set question on Windows (Windows root store vs
`cacerts`) is real, is NOT answered here, and has no measurement yet.

### The ordering walk, and what it is honestly for

`SSLSession.getPeerCertificates()` is ORDERED — the peer's own certificate,
then each issuer. On Unix that is free: OpenSSL returns a LIST, in the order
the peer sent it. Here the source is a `CertStore`, which is a SET; `certs()`
enumerates it in whatever order the store holds and Windows contracts nothing
about that order. So the path is walked rather than hoped for.

**MEASURED, and worth stating plainly rather than dressing up:** with the walk
skipped, live SChannel already returned leaf-first on all six hosts of
`PeerChainOrderProbe` — output identical, CN for CN. This is therefore *not* a
repair of an observed defect. It is the removal of a dependence on an order
the platform does not promise.

Which is exactly why the walk is a plain function (`order_chain_from_leaf`,
generic over a `(subject, issuer)` lookup, no `#[cfg]`) with its own tests over
a **shuffled** set: the live store declines to produce the input that would
exercise it, so a probe against the real world cannot tell the walk from its
absence. Six unit tests do, and disabling the walk fails exactly three of them
— the shuffled path, the off-path certificate, and the unparseable member —
while the three that must not change (already-ordered, empty pool, self-issued
root) still pass.

That discrimination is the point. The day before, on the Unix side, a
trust-anchor relaxation shipped with a test that passed identically with the
fix reverted; the break-run is what caught it. Same discipline, applied before
landing this time.

## Verified

Windows box, `--java-home <Temurin 25.0.3> --nojit`, branch off `origin/dev`.

* the table at the top, every row, both arms from one binary;
* `PeerChainOrderProbe` CN-for-CN identical to HotSpot on 6 hosts;
* `cargo test -p cratonvm-native-builtins` on **Windows**: 4069 passed / 23
  failed — the same 23 that fail on Linux, name for name, all pre-existing and
  none in `servlet`, `x509_manager`, `crypto_impl` or `tls`;
* `cargo test` on **Linux**: 4083 passed / 23 failed, same list. Linux is
  unaffected by construction (the new code is `#[cfg(windows)]`) and was
  re-measured anyway, because this change edits files the Unix path shares:
  `RealChainProbe` 20/20, `CustomTmProbe` 3/3/4, `PeerChainOrderProbe`
  identical to HotSpot;
* netty's 78 SSL/TLS classes on Linux. One class more than the earlier
  baseline read as FAIL and **was not**: `JdkSslRenegotiateTest` timed out
  against a 30 s budget after 122 s of wall clock, on a box carrying load
  average 17. Re-measured interleaved, fixed/native-tls × 3 reps: PASS on
  every rep of both arms, 3–22 s. The two classes that do fail
  (`OpenSslKeyMaterialManagerTest`, `SslContextBuilderTest`) fail **identically
  on both arms of every rep** — pre-existing, and one of them wants a
  `netty_tcnative` this host cannot load.

  That is the second time in two days a wall-clock timeout on a shared box
  read as a regression. A netty verdict taken while anything else is building
  is not a verdict.

## Repro
```bash
javac -d <probe-dir> probes/CustomTmProbe.java probes/RealChainProbe.java \
                     probes/PeerChainOrderProbe.java
<cratonvm.exe> --java-home $JDK25 --nojit -c <probe-dir> RealChainProbe   # 20 accept
CRATONVM_TLS_OPENSSL_CLIENT=0 <cratonvm.exe> … RealChainProbe             # 0 accept, len=1
```

## What is still open

Nothing on this page. Two adjacent questions it deliberately does not answer,
neither measured:

* the Windows **anchor set** — the platform root store vs the JDK's `cacerts`,
  the same divergence the sibling page closed on Unix. Not expressible through
  SChannel the way it was through OpenSSL, per "deliberately narrower" above;
* the negotiated **protocol and cipher suite** on this path still come from
  compile-time constants. The `schannel` crate exposes no accessor for either,
  so they are unchanged rather than newly approximate — the OpenSSL bridge
  reports the real values because OpenSSL can be asked.

## Related
* `tls-client-captures-only-the-leaf-so-a-custom-trustmanager-cannot-validate-20260816-FIXED-20260817.md`
  — the Unix fix this is the other half of, its measurements, and the two
  further defects it uncovered.
* `tls-client-trust-is-openssl-seclevel-not-the-jdk-trustmanager-20260816-FIXED-20260817.md`
  — the security-level residue, Unix only; SChannel has no SECLEVEL, so that
  band does not exist here.
