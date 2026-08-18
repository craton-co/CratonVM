# netty OpenSSL key material: the opaque-key gap, and what is left of it

**Status:** OPEN on **D** only. Sections A (`KEY_VALUES_MISMATCH`) and C
(`SslContextBuilder` accepting an invalid cipher) closed earlier; **A′** closed
2026-08-17 (an EKU read as an eligibility filter — see §A′); **B closed
2026-08-18** — the verifier-time trust check is in, and the 49-test regression
that withdrew it on 2026-08-17 is gone.

**What closed B was not what the handover predicted.** It named its fifth
blocker "the TrustManager is invoked TWICE" and asked for the invocation count
to be instrumented as the next measurement. The manager runs ONCE; the engine
had two sessions where JSSE has three. Reading the failing test cost ten
minutes and the instrument would have answered a question that was not the
one to ask. See §B.

Measured on Azure host 2 (Linux x86_64, JDK 25), one class per process,
`-XX:+UseG1GC`, with `netty-tcnative-boringssl-static` on the classpath for
BOTH VMs. If a class in this suite aborts inside `gc/`, check that first
rather than reading it as a TLS defect.

| class | dev 2026-08-15 | 2026-08-13 page (`e5f4597e6`) | 2026-08-17 | HotSpot 25 |
|---|---|---|---|---|
| `JdkSslEngineTest` | 707 / 821, 48 failed | 755 / 821, 0 failed ✅ | — | 755, 0 failed |
| `JdkDelegatingPrivateKeyMethodTest` | 1 / 27 | 10 / 27 | **27 / 27** ✅ | 27 / 27 |
| `OpenSslPrivateKeyMethodTest` | 24 / 24 | 24 / 24 | 24 / 24 | 3 / 24 |
| `SslHandlerTest` | 47 / 54 | 48 / 54 | **53 ok + 1 aborted** (Linux); see §B for the Windows read | 53 ok + 1 aborted |
| `SslContextBuilderTest` | 21 / 21 | 21 / 21 ✅ | 21 / 21 ✅ | 21 / 21 |
| `ParameterizedSslHandlerTest` | never finished, or hangs | 61–63 / 63; still stalls intermittently, quiet host or not | not re-measured | 63 / 63 |

The 2026-08-17 column is `fix/netty-nio-pcap-tls-residuals-20260817` on this same
host. `JdkDelegatingPrivateKeyMethodTest` measured 27/27, 26/27, 27/27 across
three consecutive runs at host load 27 / 21 / 19; the single failure in the
middle run is a `TimeoutException` against the class's own `@Timeout(30)` on an
RSA-PSS SHA512 row, and it landed on a DIFFERENT parameterisation each time it
appeared (`[8] ...true`, then `[19] ...false`), which is load, not a defect. Its
14 real failures before the fix were exactly the 11 + 3 rows this page attributed
to "the client sends no certificate".

`JdkSslEngineTest` is now the oracle exactly and its page is retired — see the
retired `jdksslenginetest-engine-level-gaps` write-up for the four causes that
closed it, three of which (PSS parameters, anchor-is-not-path-validated, JCA
`Signature`) are shared with this page.

`OpenSslPrivateKeyMethodTest` passes 24 where HotSpot passes 3 — that
direction is a claim to check, not a win to bank. HotSpot's 21 failures on
this host are netty-tcnative's own; the number is recorded so the next reader
does not mistake the gap for progress.

## ~~A′~~ — FIXED 2026-08-17. The client had no alias, because an EKU was read as an eligibility test

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

**The residual was 14 of 27** (this page said 17; a re-measurement on
2026-08-17 found 14, i.e. dev had already picked up three of the ECDSA rows).
It split cleanly along one axis:

| n | parameterisations | error |
|---|---|---|
| 11 | every `clientUsesProvider=true` | `SSLV3_ALERT_HANDSHAKE_FAILURE` — the client sends no certificate |
| 3 | `testClientServerScenarios` 1–3 | `TLSV1_ALERT_CERTIFICATE_REQUIRED`, same shape |

**"The client sends no certificate" was literal, and the cause was one line
above the TLS layer.** `chooseClientAlias` answered `null`, so netty's
`OpenSslKeyMaterialProvider` had nothing to present.

`build_key_manager_state` used `is_client_cert`/`is_server_cert` to decide
MEMBERSHIP of `client_aliases_by_key_type` / `server_aliases_by_key_type`, and
both predicates reject a certificate whose ExtendedKeyUsage is present but does
not name their role. This fixture builds exactly one certificate —

```java
.setKeyUsage(true, CertificateBuilder.KeyUsage.digitalSignature)
.addExtendedKeyUsageServerAuth()      // EKU = { id-kp-serverAuth } ONLY
```

— and uses it on BOTH sides. So the alias was a server alias and never a client
alias, and a client with no alias sends no certificate; a `ClientAuth.REQUIRE`
server answers with exactly the two alerts above.

**JSSE does not filter that way.** The default `KeyManagerFactory` algorithm is
`SunX509`, and `SunX509KeyManagerImpl.getAliases` filters on the key's algorithm
and (when the peer sends one) the issuer list only — KeyUsage and EKU are never
consulted. `X509KeyManagerImpl` (NewSunX509) does look at them, but it RANKS on
the result and still answers. Measured on JDK 25, same certificate, same
classpath, both VMs (`/data/nssl/KmAliasProbe.java`):

```
HotSpot   SunX509     getClientAliases(RSA)=[key]      chooseClientAlias(RSA)=key
HotSpot   NewSunX509  getClientAliases(RSA)=[1.0.key]  chooseClientAlias(RSA)=3.0.key
CratonVM  SunX509     getClientAliases(RSA)=[]         chooseClientAlias(RSA)=null
CratonVM  NewSunX509  getClientAliases(RSA)=[]         chooseClientAlias(RSA)=null
```

**Fix.** Both population sites push every private-key alias into BOTH lists, in
two passes so a role-matching certificate still comes first and a caller taking
`.first()` keeps preferring it. The predicates themselves are unchanged and are
re-documented as preferences; their tests are renamed `..._requires_...` →
`..._prefers_...` so the names stop asserting the old contract.
`an_eku_mismatch_orders_an_alias_last_but_never_drops_it` pins both halves.

**Read this as a shape, not just a fix.** A capability predicate borrowed from
the certificate-validation vocabulary (`is_client_cert`) reads like an
eligibility test and was used as one; the platform it has to imitate treats the
same information as a preference. When a native reimplements a JDK selection
policy, the thing to check is not whether the predicate is *correct* but whether
the original is allowed to answer *no*.

`keystore::engine_set_key_entry`'s silent drop — the other cheap move this page
asked for — is closed too: the `if key_der.is_empty() { return Ok(None) }` arm now
names the store, alias, algorithm and format, and says the live-KeyStore
enumeration path keeps such a key by reference. An opaque provider key is the
NORMAL case for that branch, so the silence was hiding the routine case, not an
exotic one.

## ~~B~~ — `SslHandlerTest` — CLOSED 2026-08-18

`testHandshakeFailureOnlyFireExceptionOnce` was the one real residual: the
SERVER's handshake succeeded where it must fail, because the client's
`TrustManager` rejection was consulted after the handshake and its alert
arrived behind its own `Finished`. The destination — consult the manager
INSIDE `verify_server_cert`, so rustls aborts before producing `Finished` —
was proven on 2026-08-17 and then withdrawn: it cost 49 of
`JdkSslEngineTest`'s 821 tests.

It is in now, ABBA on `SslHandlerTest`:

```
ARM ctl : SniClientTest=3/0   SslHandlerTest=36/16   onlyFireOnce FAILED
ARM fix : SniClientTest=3/0   SslHandlerTest=37/15   onlyFireOnce passes
ARM fix : SniClientTest=3/0   SslHandlerTest=37/15   onlyFireOnce passes
ARM ctl : SniClientTest=3/0   SslHandlerTest=36/16   onlyFireOnce FAILED
```

and on the gate that withdrew it — `JdkSslEngineTest`, 821 tests, ABBA with
the per-method budgets lifted (`-Djunit.jupiter.execution.timeout.mode=disabled`,
this page's own advice), on a box at load 1.4–2.2:

| arm | ok | failed | `expected: <0> but was: <1>` |
|---|---|---|---|
| control | 754, 753 | 1, 2 | **0** |
| fix | 753, 754 | 2, 1 | **0** |

Every failure on either arm is `testSSLSessionId` or
`testMutualAuthSameCertChain`, both of which appear on the CONTROL too and
neither of which appears on every run — one shared flaky pair, not a movement.
The withdrawn attempt was **706 ok / 49 failed**.

### The fifth blocker was a wrong question

The four solved blockers stand unchanged: the registry lock (`ConnCheckout`),
`handshake_status_of` reading a checked-out connection as NOT_HANDSHAKING, the
engine `ObjectRef` needing to be a pin handle, and `getHandshakeSession()` /
`getSession()` sharing bindings. The fifth was stated as:

> **UNSOLVED: the TrustManager is invoked TWICE.** … Seeing 1 means the manager
> already ran once on that engine and bound the key. … **instrument the count
> first.**

It is invoked once. What the failing test does
(`SSLEngineTest.testSessionAfterHandshake0`):

```java
clientEngine.getSession().putValue(key, Boolean.TRUE);   // BEFORE the handshake
…
handshake(…);
// and inside checkServerTrusted, mid-handshake:
assertEquals(0, engine.getHandshakeSession().getValueNames().length);
```

The manager was seeing a binding **the test itself had made a few lines
earlier**. `engine_session_table` was keyed `(engine, handshaked: bool)`, and
that bool has to carry three states:

* `Fresh` — `getSession()` before anything is negotiated: JSSE's null session,
  with its own bindings;
* `Handshaking` — `getHandshakeSession()`, the session being negotiated;
* `Negotiated` — `getSession()` afterwards, which in JSSE **is** the session
  that was being negotiated.

`Fresh` and `Handshaking` were one object. Invisible while the manager ran
after the handshake — it only ever saw `Negotiated`, which is freshly built —
and immediate once it ran inside one. The binding-carry added in the withdrawn
attempt then made it worse by copying the null session's bindings forward,
which is precisely what the test asserts must not happen ("The values should
not have been carried over").

**The phase is a function of the DOOR, not of the connection state.**
`SSLEngine.getSession()` during a handshake answers the *current* session,
which before the first successful one is still the null session, while
`getHandshakeSession()` answers the pending one. No predicate over
`is_handshaking()` alone can express that, which is why the two-state key could
not be repaired in place. `session_phase(door, handshaked)` is a free function
with its own tests for the same reason: the defect was a missing distinction,
not a wrong branch.

### Re-landing it: a revert is not a rebase

The withdrawn work was on `fix/netty-tls-verifier-time-trust-20260817`, and the
obvious way back in is to revert the revert. **It applies cleanly and is
wrong.** `git revert` restores a FILE STATE, and `t27_tls.rs` had moved on:
dev had since changed `engine_run_sni_match_check` to answer "refused?" and
taught `do_unwrap` to consume the hello and report NEED_WRAP, so the caller
comes back for the wrap that emits the `unrecognized_name` alert. The revert
silently took that away — no conflict, because the two changes touch different
lines of the same function.

MEASURED, and the reason it was caught rather than shipped: the 78-class netty
SSL sweep put `SniClientTest` at 3/0 on the control and 2/1 on the build, ABBA,
deterministic —

```
expected: <javax.net.ssl.SSLException>
     but was: <io.netty.channel.StacklessClosedChannelException>
```

— and a `CRATONVM_DBG_TLS_HS=1` trace of both arms named it exactly: the
control logs `do_unwrap id=3 RETURN(sni-refused) … hs=NEED_WRAP`, then a
`do_wrap` that produces the 7-byte alert record, then the client's
`received fatal alert: UnrecognisedName`. The reverted build logs none of those
three.

Re-landed as CHERRY-PICKS of the four blocker commits onto current `dev`
instead, so later fixes in the same function survive. The two session commits
were deliberately NOT picked — the three-phase change above supersedes them.

### What to keep from this

* **A count answers the question you asked, not the one you needed.** "Saw 1,
  so it ran twice" is a sound inference from a false premise — that two names
  referred to two objects. The test source held the answer.
* **A clean revert can still drop a later fix.** Nothing conflicts, nothing
  warns, and the loss is inside a function body. When re-landing withdrawn
  work, cherry-pick the changes; do not restore the file.
* **Read this class per-MODE, not per-count.** On the first pass the build
  showed `TimeoutException` on `testMutualAuthSameCertChain` (30 s) and
  `tearDown()` (120 s) where the control showed none, and ran 368 s against
  273 s. With the budgets lifted both arms fail the same one test. Slower, not
  broken — a count alone reads it as a regression.
* **A class the local box cannot run is not a class you can skip.**
  `JdkSslEngineTest` exceeds 900 s on the Windows box where HotSpot takes
  180 s, so it must be measured on Azure.
* `a_verifier_time_rejection_reaches_the_server_while_it_is_still_handshaking`
  (on `dev`) proves the destination and carries its own accepting control arm.

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
