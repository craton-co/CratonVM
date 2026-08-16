# netty OpenSSL key material: the opaque-key gap, and what is left of it

**Status:** OPEN, and down to two named defects plus two unexamined
`SslHandlerTest` behaviours. Sections A (`KEY_VALUES_MISMATCH`), C
(`SslContextBuilder` accepting an invalid cipher) and the
`AbstractMethodError` half of A′ are fixed; the **JCA half of A′ is fixed**
(2026-08-15) and what remains of A′ is netty's OPENSSL **client** path;
section B is halved; section D still hangs on a LOADED host but finishes on
a quiet one, and two failures it was hiding were an unpinned array — fixed.

Measured on Azure host 2 (Linux x86_64, JDK 25), one class per process,
`-XX:+UseG1GC`, with `netty-tcnative-boringssl-static` on the classpath for
BOTH VMs. If a class in this suite aborts inside `gc/`, check that first
rather than reading it as a TLS defect.

| class | dev 2026-08-15 | now (`e5f4597e6`) | HotSpot 25 |
|---|---|---|---|
| `JdkSslEngineTest` | 707 / 821, 48 failed | **755 / 821, 0 failed** ✅ | 755, 0 failed |
| `JdkDelegatingPrivateKeyMethodTest` | 1 / 27 | **10 / 27** | 27 / 27 |
| `OpenSslPrivateKeyMethodTest` | 24 / 24 | 24 / 24 | 3 / 24 |
| `SslHandlerTest` | 47 / 54 | **48 / 54** | 53 / 54 |
| `SslContextBuilderTest` | 21 / 21 | 21 / 21 ✅ | 21 / 21 |
| `ParameterizedSslHandlerTest` | never finished, or hangs | **61–63 / 63; still stalls intermittently, quiet host or not** | 63 / 63 |

`JdkSslEngineTest` is now the oracle exactly and its page is retired — see the
retired `jdksslenginetest-engine-level-gaps` write-up for the four causes that
closed it, three of which (PSS parameters, anchor-is-not-path-validated, JCA
`Signature`) are shared with this page.

`OpenSslPrivateKeyMethodTest` passes 24 where HotSpot passes 3 — that
direction is a claim to check, not a win to bank. HotSpot's 21 failures on
this host are netty-tcnative's own; the number is recorded so the next reader
does not mistake the gap for progress.

## A′ — the JCA half is fixed; netty's OPENSSL *client* path is not

**What A′ was.** `KeyManagerFactory.getKeyManagers()` answered with an object
stamped with the `javax/net/ssl/X509KeyManager` INTERFACE's class id, every
method abstract, whenever `keystore_id_from_object` returned 0 →
`AbstractMethodError` from `OpenSslKeyMaterialProvider.chooseKeyMaterial`.
Fixed 2026-08-15 by enumerating ANY `KeyStore` through its own bytecode and
keeping an opaque key BY REFERENCE.

**What was behind it, and is now fixed.** The residual was
`DATA_LEN_NOT_EQUAL_TO_MOD_LEN` — the signature `JdkDelegatingPrivateKeyMethod`
produced through `Signature.getInstance(alg, provider)` was not
modulus-length. Four JCA defects, each measured against HotSpot with
`apps/.../SigProbe.java` and `SigProbe2.java` (netty's own
`findCompatibleSignature` loop against a `Security.addProvider`-registered
provider):

1. **Only `SHA256withRSA` could sign.** `getInstance` advertised
   `SHA1withRSA`, `SHA384withRSA`, `SHA512withRSA` and `MD5andSHA1withRSA`,
   `initSign` accepted the key, and `sign()` then refused with "this VM has no
   native implementation for that algorithm". The VERIFY side had been
   digest-parameterised all along.
2. **`initSign` accepted a key it could not use.** HotSpot answers
   `InvalidKeyException: Missing key encoding` at init for an opaque
   `PrivateKey`; that exception is how netty's provider search learns to try
   the next provider. Accepting it and refusing at `sign()` made the search
   stop at the first provider and commit to one that can never work.
3. **A third-party provider's `SignatureSpi` was never instantiated.**
   `Security.addProvider` + `put("Signature.X", MySpi.class.getName())` was
   recorded (it showed up in `getServices()` and in the offered-name gate) but
   the class was never constructed, so every such `Signature` was quietly
   serviced by the built-in native engine. For a key only that provider can
   use — which is the entire reason to register one — that cannot work.
   Providers this VM services natively are excluded by name, so no built-in
   engine is bypassed.
4. **`setParameter(PSSParameterSpec)` was a no-op.** PSS signed and verified
   under the algorithm-name default whatever spec was installed: sign with
   SHA-256/salt-32 then verify with SHA-512/salt-64 answered `true` here and
   `false` on HotSpot.

`SigProbe2` now matches HotSpot line for line, including the
`NoSuchAlgorithmException` for `MD5andSHA1withRSA` (the mock provider does not
register it).

**The residual (17 of 27).** It splits cleanly along one axis:

| n | parameterisations | error |
|---|---|---|
| 11 | every `clientUsesProvider=true` | `SSLV3_ALERT_HANDSHAKE_FAILURE` — the client sends no certificate |
| 3 | `testClientServerScenarios` 1–3 | `TLSV1_ALERT_CERTIFICATE_REQUIRED`, same shape |
| 3 | ECDSA with `clientUsesProvider=false` | separate; not yet examined |

All eight RSA `clientUsesProvider=false` rows pass, as do
`testMultipleHandshakes` and `testAlgorithmCaching` — so the SERVER-side
opaque key works end to end and the JCA layer is no longer the constraint.
What fails is netty's OPENSSL **client** key material:
`SslContextBuilder.forClient().keyManager(opaqueKey, "", cert)` with
`USE_JDK_PROVIDER_SIGNATURES`, against a server with `ClientAuth.REQUIRE`.
`CRATONVM_DBG_TLS_AUTH=1` shows the live-keystore enumeration succeeding on
that side too (`kmf(phases_late).init(live KeyStore) aliases=1`), so the next
step is downstream of the KeyManager — `OpenSslKeyMaterialProvider` for a
client, and the tcnative private-key-method callback — not in `keystore.rs`.

Two cheap moves that are still worth making: `keystore::engine_set_key_entry`
still drops an entry silently on `if key_der.is_empty() { return Ok(None) }`,
and should say something.

## B — `SslHandlerTest`, 52 / 54 (HotSpot 53 / 54)

* ~~`testHandshakeFailureCipherMissmatchTLSv12Jdk` / `TLSv13Jdk`~~ — FIXED
  2026-08-16. **A wrap that raises cannot also deliver.**

  The page had this the wrong way round for a while: the symptom is
  `StacklessClosedChannelException` where an `SSLException` belongs, and the
  server was assumed to be the side missing its exception. It is not — the
  assertion that fails is the CLIENT's (`SslHandlerTest:1670`), and the server
  raised correctly all along.

  `do_wrap` drained rustls's fatal alert into the caller's `dst`, then
  returned `Err`. netty advances its out-buffer's writerIndex from
  `result.bytesProduced()`, and a throwing `wrap` has no result to read it
  from — so the buffer read back empty, was released, and the alert died
  there. The client received not one byte after its ClientHello and failed on
  the channel closing. The trace that names it: server `do_unwrap` NEED_TASK
  consumed=138, `task.run … rustls: peer is incompatible:
  NoCipherSuitesInCommon`, `do_unwrap … hs=NEED_WRAP`, then a `do_wrap` that
  logs its DST and never logs a RESULT (it threw), followed by a second
  `do_wrap … produced=0` — the alert gone.

  Two lines fix it, and they belong together:

  * the deferred failure is taken only when `drained.is_empty()` as well, so
    the alert leaves on an ordinary result the caller can act on;
  * `handshake_status_of` keeps answering NEED_WRAP while a failure is
    pending, so the caller comes back for the wrap that produces nothing —
    and that one raises. netty's `wrapNonAppData` loops on NEED_WRAP and
    `ctx.write`s each iteration that produced bytes, so the alert is already
    on the wire when the throw arrives.

  Measured ABBA on a quiet host: 50/54 → 52/54, both arms twice, and both
  tests pass alone.
* ~~`testTruncatedPacket`~~ — FIXED. It needed `SSLProtocolException`, not
  `SSLHandshakeException`: a protocol violation (a ServerHello pushed INTO a
  server engine) is a different class from a certificate or negotiation
  failure, and JSSE's `SSLEngineInputRecord` raises exactly that.
  `do_unwrap`'s handshake-error arm now picks the class from the rustls error
  kind (`InappropriateMessage`, `InappropriateHandshakeMessage`,
  `InvalidMessage`, `PeerMisbehaved`, `PeerSentOversizedRecord`,
  `BadMaxFragmentSize` → `SSLProtocolException`; everything else stays
  `SSLHandshakeException`), on both the immediate client throw and the
  deferred server one.

  It passes in the CLASS run and fails when run ALONE — on pristine dev as
  well as on this branch, measured 2026-08-16. So it is a per-process latch,
  not a residual of the class-run fix, and a solo `#testTruncatedPacket`
  result says nothing about it either way.
* `testHandshakeFailureOnlyFireExceptionOnce` — `expected: <false> but was:
  <true>`; unexamined.
* ~~`testClientHandshakeTimeoutBecauseExecutorNotExecute` /
  `testServerHandshakeTimeoutBecauseExecutorNotExecute`~~ — FIXED 2026-08-16.
  The engine implements JSSE's delegated-task contract now; `DelegatedTask` in
  `t27_tls.rs` carries the whole shape. `getDelegatedTask()` was not
  registered at all, so a caller that followed the NEED_TASK this engine could
  ALREADY report (the fallthrough at the end of `handshake_status_of`) had
  nothing to collect.

  **Where the deferral goes.** On the FIRST inbound handshake flight, on
  either side — never on a `wrap`. A fresh client's first `wrap` must emit the
  ClientHello and answer NEED_UNWRAP
  (`SSLEngineTest.testSSLEngineUnwrapNoSslRecord` asserts exactly that, on all
  12 parameterisations), and real JSSE defers the certificate and
  key-agreement work that follows the ServerHello, not anything before the
  ClientHello.

  **What the task does.** It PROCESSES the flight — `read_tls` plus
  `process_new_packets`, and `engine_begin` too on a server, whose connection
  does not exist until then. Staging the records and leaving them for a later
  `unwrap` is not enough: netty makes that unwrap (`unwrapNonAppData` passes
  an empty buffer) but `SSLEngineTest.handshake` only unwraps when it has
  bytes to feed, and the bytes were consumed by the call that answered
  NEED_TASK — both engines then sit in `wrap` producing nothing, each waiting
  for the other, and `testSessionCacheTimeout` spins where the control takes
  1.85 s.

  **Three rules the callers impose:**

  1. *Consume first, then defer.* netty's `SslHandler.decodeJdkCompatible`
     hands `unwrap` exactly one TLS record and treats
     `bytesConsumed != packetLength` as "not an SSL/TLS record" — it throws
     `NotSslRecordException` and fails the handshake.
  2. *Never answer `null` to a caller that was promised a task.* netty's
     `SslTasksRunner.run()` returns immediately when `getDelegatedTask()` is
     null, WITHOUT calling `runComplete()`, so `SslHandler` stays in
     `STATE_PROCESS_TASK` — where `decode()` and `flush()` are both no-ops —
     and the connection is wedged for good.
  3. *The task never throws.* It runs on somebody else's thread and callers do
     not treat it as a call site that can fail; throwing left `delegate=true`
     blind to failures `delegate=false` saw. Failures go on
     `EngineState::deferred_handshake_error`.

  Two things the deferral routes past `do_unwrap`'s record loop and therefore
  has to redo itself: the ServerHello session-id peek (`testSSLSessionId`
  compares the two engines' ids byte for byte), and nothing else — the ALPN
  selector already runs ahead of it.

  Measured against pristine dev, interleaved ABBA on a quiet host:
  `SslHandlerTest` 48/54 → 50/54 (twice each), `SslContextBuilderTest` 21/21
  both, and `JdkSslEngineTest` — 821 tests, the `delegate=true` surface this
  page asked to be checked — `ok=755 failed=0` on both arms of the new build
  against `754/1` and `755/0` on the control.

  **Read `JdkSslEngineTest` only from a quiet host.** At load 15–30 it
  produced four `testTlsExtension` failures and a hang that survived three
  rebuilds and looked exactly like an ALPN defect; at load 6–8 the same binary
  is clean twice over, and `testTlsExtension` alone is 12 ok / 12 aborted on
  both arms at any load.

## ~~C~~ — `SslContextBuilder` accepts an invalid cipher — FIXED 2026-08-15

`SSLEngine.setEnabledCipherSuites` validates its argument and throws
`IllegalArgumentException` for a name that is not a cipher suite.
`SslContextBuilderTest` is 21/21.

## D — the hang is gone; a wrong object identity was behind it

`ParameterizedSslHandlerTest` finished in **six consecutive runs on a quiet
host** (114–184 s) at 61–63 of 63 — including one clean 63/63. **It is not
cured**: two later runs at host load 45–50 gave one stall, killed at the 900 s
cap, at the same parameterisation the original page named
(`reentryOnHandshakeCompleteNioChannel`, `5: clientProvider=OPENSSL_REFCNT,
serverProvider=OPENSSL_REFCNT`). That matches the original characterisation
exactly ("on a loaded host it hangs instead — 3 of 7 runs"), so the honest
reading is that the fixes below removed real failures and made the class
*usually* finish, not that the stall is gone. Any future claim about it needs
the host's load average recorded beside the result.

**It is not attributable to any VM change on this branch, and there is now an
A/B that says so (2026-08-16).** At host load 13–30, `pristine dev` stalls at
`reentryOnHandshakeCompleteNioChannel` after 21 of 63, and the delegated-task
branch stalls at the same test after 24 and 25 of 63 — three arms, one
symptom, one of them the control. At load ~4 the same control finishes 63 in
176 s. Load is the variable; run the control in the same window or the result
is unreadable. The prior page's two
contributing findings stand: the `Selector.select() returned prematurely 512
times in a row` storm came from `nio_selector.rs`'s interest-ops nudge (whose
Linux premise was false) and is gone, and the storm was a symptom rather than
the cause.

### What the hang was hiding — an unpinned array, FIXED 2026-08-15

Two intermittent failures shared one call site,
`ReferenceCountedOpenSslServerContext.newSessionContext` →
`toBIO(alloc, manager.getAcceptedIssuers())`, and one root cause:

```
java.lang.NoSuchMethodError: 'byte[] sun.security.util.DerValue.getEncoded()'
    at io.netty.handler.ssl.PemX509Certificate.append(…:126)
java.lang.IllegalArgumentException: Null element in chain: [null × 32]
    at io.netty.handler.ssl.PemX509Certificate.toPEM(…:80)
    (netty wraps this one as "SSLException: unable to setup trustmanager")
```

Both `getAcceptedIssuers` implementations —
`x509_manager::get_accepted_issuers` and the `javax/net/ssl/X509TrustManager`
one in `t27_tls` — built the result array and then filled it in a loop whose
body ALLOCATES (a mirror object, two strings, a DER `byte[]`). The array
reference was held raw, so a moving young collection landing inside the loop
relocated it and every `set_array_element` after that wrote into the vacated
slots. What the live array kept was whatever the collector left there: usually
`null` (32 of them — the system trust-anchor count), occasionally a `DerValue`
from the certificate parsing the loop had just done, which is why one site
produced two unrelated-looking errors. Same family as
`t27_tls::attach_trust_managers_to_ctx`'s documented GC fix: a native local
held live across an allocation. Both loops now pin and re-read.

A `sun.security.util.DerValue.getEncoded()` alias for `toByteArray()` was
added alongside — JDK 25's class genuinely has no `getEncoded()` (verified
with `javap --module java.base`), and a `DerValue` that wraps a parsed
certificate carries exactly the DER `X509Certificate.getEncoded()` is
contracted to return, so the alias is value-correct. It is belt-and-braces,
not the fix: it changes no object's identity, and any other `X509Certificate`
method asked of such an object would still fail.

**What is left:** 61–62 of 63, and neither error above appears in any run.

### The residual: intermittently unusable OPENSSL key material

One test method, `reentryOnHandshakeCompleteNioChannel`, one failure per run,
and **three different errors across runs** — all in netty's OPENSSL
key-material path, all on an `OPENSSL`/`OPENSSL_REFCNT` server:

```
OpenSslHandshakeException: error:100000ae:…:NO_CERTIFICATE_SET
SSLHandshakeException:     Unable to find key material for auth method(s):
                           [ECDHE_ECDSA, ECDHE_RSA, …, RSA]
SSLException:              PrivateKey type not supported PKCS#8
```

The third is the informative one. It comes from
`OpenSslKeyMaterialProvider.validate`, whose `catch` prints
`key.getFormat()` — so the key DID report `PKCS#8`, and what failed inside the
`try` was `toBIO(alloc, key)` → `SSL.parsePrivateKey`. A key that answers
`getFormat()` correctly and then does not parse is a **value** problem, not a
type one; together with the null/`DerValue` array corruption fixed above, the
shape to suspect first is another native local held live across an
allocation — a `byte[]` this time (`getEncoded()`, or the PEM built from it),
not a reference array.

**An array-rooting sweep did NOT close it.** `KeyStore.getCertificateChain`,
`KeyStore.aliases`, `SSLSession.getPeerCertificates` and
`SSLSession.getLocalCertificates` all had the same unpinned-array defect and
were converted to `util_concurrent_ext::build_rooted_ref_array` (which exists
now, and is the right thing to reach for). Measured A/B on a quiet host: one
failure per run on BOTH the control and the fixed build, only the error text
differing. So those four were real defects worth fixing, and none of them is
this one.

**Correction to the load story.** The stall is NOT load-only: with the host at
load 5.7 a run still hung past 8 minutes at the same
`reentryOnHandshakeCompleteNioChannel` parameterisation. The earlier "finishes
on a quiet host" reading came from too few samples. Treat the class as
intermittently hanging, full stop, and record the load average beside any
result from it.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.handler.ssl.JdkDelegatingPrivateKeyMethodTest \
  io.netty.handler.ssl.OpenSslPrivateKeyMethodTest \
  io.netty.handler.ssl.SslHandlerTest io.netty.handler.ssl.SslContextBuilderTest \
  io.netty.handler.ssl.ParameterizedSslHandlerTest > /tmp/km.txt
CV_BIN=bin/cratonvm-netty-zgc bash run-netty-suite.sh --list /tmp/km.txt --gc g1 --shards 1 --timeout 600 --out /tmp/repro
```

`common.args` MUST carry `netty-tcnative-boringssl-static-<ver>-<os>.jar`.
Without it `OpenSsl.isAvailable()` is false, the OPENSSL parameters are never
generated, and these classes read as clean passes while running a fraction of
their tests. The netty Maven reactor resolves the *dynamic* `netty-tcnative`
artifact on Linux, whose `.so` needs `OPENSSL_3.2.0`; on a host with an older
`libssl.so.3` neither VM can load it.

`CRATONVM_DBG_TLS_AUTH=1` prints the `KeyManagerFactory.init` keystore id, the
alias count of a live-enumerated `KeyStore`, and — new — every client- and
server-side TLS session-store operation, which is how a failed resumption is
told apart from a ticket that was never issued.

## Related

- the retired `jdksslenginetest-engine-level-gaps` write-up — the same
  package's JDK-engine gaps, closed to the oracle in the same session.
- the retired `ssl-suite-test-discovery-undercounts` and
  `ssl-cert-validation-residuals` write-ups — the work that made these
  reachable.
