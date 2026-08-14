# netty OpenSSL key material: the opaque-key gap, plus three JDK-engine residuals

**Status:** OPEN but much smaller than when this page was written. The
`KEY_VALUES_MISMATCH` family (section A, 5 classes) was root-caused and FIXED on
2026-08-14 — see "What was section A" below. What is left is one distinct
key-material defect (A′) and three unrelated single-behaviour gaps (B, C, D).

Measured on Azure host 2 (Linux x86_64, JDK 25), `--shards 4 --timeout 400`,
`-XX:+UseG1GC`, with `netty-tcnative-boringssl-static` on the classpath for BOTH
VMs. **Use G1 for this suite**: ZGC aborts several of these classes for an
unrelated reason, tracked in
`docs/known-issues/zgc-relocate-cursor-panic-on-netty-tls-20260814.md`.

## What was section A — FIXED 2026-08-14

`Error setting certificate (…:KEY_VALUES_MISMATCH)` from
`OpenSslKeyMaterialManager.setKeyMaterial`, and its sibling `Unable to load
certificate key (error:00000000:invalid library (0))`, were one defect:
`x509_manager::make_private_key_mirror` packed a **KeyManager** id into field 3
of a four-slot `java/security/PrivateKey` proxy, and
`keystore::private_key_der_from_proxy` read that field as a **keystore** id.
The two id spaces are independent counters, so
`KeyManager.getPrivateKey(alias).getEncoded()` was right only while the two
happened to be aligned — and otherwise returned either an EMPTY array (lookup
miss → a PEM with no body → `PEM_read_bio_PrivateKey` returns NULL with an empty
OpenSSL error queue, hence the uninformative `invalid library (0)`) or a
DIFFERENT valid key from another store (→ `KEY_VALUES_MISMATCH`). Netty's
`OpenSslCachingX509KeyManagerFactory.newProvider` calls `getKeyManagers()`
twice, which was enough to knock them out of alignment.

Fixed by tagging the composite (`KM_PROXY_TAG`, bit 62) so the reader can tell
the two conventions apart. Landed with a second fix in the same area:
`KeyManagerFactory.getInstance("PKIX"|"NewSunX509")` now hands out
`sun.security.ssl.X509KeyManagerImpl` rather than reporting the `SunX509` class
for every algorithm — netty branches on exactly that name to decide whether
alias-keyed caching is legal.

| class | before | after | HotSpot 25 |
|---|---|---|---|
| `SniHandlerTest` | 40 / 50 | **50 / 50** ✅ | 50 |
| `PkiTestingTlsTest` | 3 / 13 | **13 / 13** ✅ | 13 |
| `OpenSslCachingKeyMaterialProviderTest` | 3 / 6 | **6 / 6** ✅ | 6 |
| `OpenSslX509KeyManagerFactoryProviderTest` | 3 / 6 | **6 / 6** ✅ | 6 |
| `ocsp.OcspTest` | 13 / 14 | **14 / 14** ✅ | 14 |

Across all 66 `handler.ssl` classes under G1: found 619 → **682**, ok 475 →
**565**, failed 88 → **61**, and no class regressed.

## A′ — `OpenSslX509KeyManagerFactory` gets a KeyManager with no methods

```
java.lang.AbstractMethodError: method javax/net/ssl/X509KeyManager
  .getCertificateChain(Ljava/lang/String;)[Ljava/security/cert/X509Certificate;
  has no Code attribute
    at io.netty.handler.ssl.OpenSslKeyMaterialProvider.chooseKeyMaterial(…:119)
    at io.netty.handler.ssl.OpenSslX509KeyManagerFactory$…$ProviderFactory
        $OpenSslPopulatedKeyMaterialProvider.<init>(…:195)
    at io.netty.handler.ssl.ReferenceCountedOpenSslContext
        .setupSecurityProviderSignatureSource(…:1134)
```

| class | CratonVM | HotSpot 25 |
|---|---|---|
| `JdkDelegatingPrivateKeyMethodTest` | 27 found, **0 ok** | 27 ok |
| `OpenSslPrivateKeyMethodTest` | 24 found, **0 ok** | 24 found, 3 ok |

(`OpenSslPrivateKeyMethodTest` fails 21 of 24 on HotSpot too; only the 3-ok gap
is CratonVM's.)

**Mechanism, confirmed by instrumentation.** `phases_late/ssl_security.rs`'s
`KeyManagerFactory.getKeyManagers()` shim has two branches: with a captured
keystore id it builds a real, natively-backed `SunX509KeyManagerImpl` mirror;
without one it falls back to `try_alloc_concurrent_synthetic("javax/net/ssl/
X509KeyManager", 0)` — an object stamped with the INTERFACE's class id, every
method of which is abstract. That branch is what these two classes reach:
`CRATONVM_DBG_TLS_AUTH=1` prints

```
[dbg-tls-auth] kmf(phases_late).init(KeyStore) this_ih=26776 ks_id=0 password_len=0
```

i.e. `keystore_id_from_object` returned 0 for the keystore netty's
`SslContext.buildKeyStore` handed to `KeyManagerFactory.init`.

**Where to look next.** The netty-shaped in-memory keystore on its own is fine —
`KeyStore.getInstance("PKCS12")` + `load(null, null)` + `setKeyEntry(alias, key,
pw, chain)` + `KeyManagerFactory.init` records `ks_id=2` and yields a working
manager (measured). What these two classes add is a **delegating private key**
(the whole point of `OpenSslPrivateKeyMethod`): a `PrivateKey` whose signing is
deferred elsewhere. `keystore::engine_set_key_entry` stores the PKCS#8 DER and
returns early — `if key_der.is_empty() { return Ok(None) }` — for a key that
does not expose one, so such an entry is silently dropped and the store never
gets an id. CratonVM's keystore stores key BYTES where the JDK stores the key
OBJECT; supporting an opaque key means holding (and GC-rooting) the Java
reference in the entry. Note HotSpot's PKCS12 keystore rejects a *fully* opaque
key outright (`KeyStoreException: Key protection algorithm not found`, from a
null `getFormat()`), so the exact key shape netty uses here still needs pinning
before the fix is designed — do that first.

Two cheap moves regardless of that answer: make the `ks_id == 0` fallback return
a natively-backed mirror with an empty registry entry instead of a
bare-interface object (`getCertificateChain` returning null is a contract-legal
answer; `AbstractMethodError` is not), and make `engine_set_key_entry`'s early
return say something rather than dropping the entry silently.

## B — cipher-mismatch handshakes close instead of raising

`SslHandlerTest.testHandshakeFailureCipherMissmatchTLSv12Jdk` / `TLSv13Jdk`
(JDK provider, no OpenSSL involved):

```
org.opentest4j.AssertionFailedError: Unexpected type,
  expected: <javax.net.ssl.SSLException>
  but was:  <io.netty.channel.StacklessClosedChannelException>
  at io.netty.handler.ssl.SslHandlerTest.testHandshakeFailureCipherMissmatch(…:1673)
```

CratonVM's JDK `SSLEngine` closes the channel on a cipher-suite mismatch where
JSSE raises an `SSLException` carrying the `handshake_failure` alert. HotSpot
passes both. `SslHandlerTest` is 46/54 (HotSpot 53/54); the other five failures
— `testTruncatedPacket`, `testHandshakeFailureOnlyFireExceptionOnce`,
`testHandshakeFailedByWriteBeforeChannelActive`, and the
`testClientHandshakeTimeoutBecauseExecutorNotExecute` /
`testServerHandshakeTimeoutBecauseExecutorNotExecute` pair — are unexamined.

## C — `SslContextBuilder` accepts an invalid cipher

`SslContextBuilderTest.testInvalidCipherJdk`: `assertThrows(
IllegalArgumentException.class, …)` gets nothing. CratonVM does not reject an
unknown cipher-suite name where the JDK provider does. One test; HotSpot passes.

## D — `ParameterizedSslHandlerTest` intermittently hangs in a selector spin

63/63 ok when it finishes, in 150–190 s. On a loaded host it hangs instead — 3 of
7 runs, at 400 s, 900 s and 1500 s caps. Traced with a per-test start/finish
listener, it always stops at the same point:

    reentryOnHandshakeCompleteNioChannel
      | 5: clientProvider=OPENSSL_REFCNT, 5: serverProvider=OPENSSL_REFCNT

and the log then repeats, forever:

    io.netty.channel.nio.NioIoHandler - Selector.select() returned prematurely
      512 times in a row; rebuilding Selector sun.nio.ch.SelectorImpl@…

JUnit's 120 s per-test timeout never fires, because
`SameThreadTimeoutInvocation` can only check after the invocation returns. This
is the NIO-channel variant — a real socket pair, not `EmbeddedChannel` — so it
is a CratonVM `Selector` question (a `select()` that returns with no ready keys
and no timeout elapsed), not an OpenSSL one. One completed run also produced a
single `SSLException: unable to setup trustmanager` on `8: clientProvider=JDK,
serverProvider=OPENSSL_REFCNT`, not seen in the other completed runs.

Whether the same spin underlies the four `*OpenSslEngine*` classes that hang at
400 s is unknown — HotSpot hangs on those four at that cap too, so they need a
raised per-class timeout before they can be judged.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.handler.ssl.JdkDelegatingPrivateKeyMethodTest io.netty.handler.ssl.OpenSslPrivateKeyMethodTest io.netty.handler.ssl.SslHandlerTest io.netty.handler.ssl.SslContextBuilderTest > /tmp/km.txt
CV_BIN=bin/cratonvm-netty-zgc bash run-netty-suite.sh --list /tmp/km.txt --gc g1 --shards 1 --timeout 400 --out /tmp/repro
```

`common.args` MUST carry `netty-tcnative-boringssl-static-<ver>-<os>.jar`.
Without it `OpenSsl.isAvailable()` is false, the OPENSSL parameters are never
generated, and these classes read as clean passes while running a fraction of
their tests — the reporting trap the retired
`ssl-suite-test-discovery-undercounts` write-up was about. The netty Maven
reactor resolves the *dynamic* `netty-tcnative` artifact on Linux, whose `.so`
needs `OPENSSL_3.2.0`; on a host with an older `libssl.so.3` neither VM can load
it.

`CRATONVM_DBG_TLS_AUTH=1` prints the `KeyManagerFactory.init` keystore id, which
is the one number section A′ turns on.

## Related

- retired `ssl-suite-test-discovery-undercounts` and
  `ssl-cert-validation-residuals` write-ups — the work that made these reachable.
- `docs/known-issues/zgc-relocate-cursor-panic-on-netty-tls-20260814.md` — run
  this suite under G1 until that is fixed.
- `docs/known-issues/netty/jdksslenginetest-engine-level-gaps-20260813.md` —
  JDK-engine gaps in the same package.
