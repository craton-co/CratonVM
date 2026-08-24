# netty OpenSSL key material: the opaque-key gap, and what closed the stall behind it

**Status: CLOSED 2026-08-20**, branch
`fix/netty-sslctx-spi-and-openssl-keymat-20260820`, Azure Linux host, release
builds from dev `86b13ed4c`. Sections A (`KEY_VALUES_MISMATCH`) and C
(`SslContextBuilder` accepting an invalid cipher) closed 2026-08-15; **A′**
2026-08-17 (an EKU read as an eligibility filter); **B** 2026-08-18 (the
verifier-time trust check, re-landed as cherry-picks); **D** 2026-08-20 — the
stall was a recursive read on the process-wide selector registry, not anything
in the key-material path at all.

**READ §D's environment note before trusting any OPENSSL number from this
host, including this page's own older ones.** With the classpath the fixture
generates, `OpenSsl.isAvailable()` is FALSE — on HotSpot too — and the way that
presents is not an error. It is `ParameterizedSslHandlerTest` passing **7 of its
63 tests** and reporting success. Anything measured without the corrected
argfile is about the JDK provider only. It is now a script,
`apps/netty-suite-runner/gen-openssl-args.sh`, plus a probe to check the answer.

**Two of this page's own instincts were wrong, and both are worth carrying
forward.** §B named its fifth blocker "the TrustManager is invoked TWICE" and
asked for the invocation count to be instrumented next; the manager runs ONCE
and the engine had two sessions where JSSE has three — reading the failing test
cost ten minutes and the instrument would have answered a question nobody
needed. §D spent two array-rooting sweeps on a theory its own A/B had already
refused twice, while the instrument that closed it — a native backtrace of a
stuck process — sat named in the VM's own watchdog output the whole time.

Measured on Azure host 2 (Linux x86_64, JDK 25), one class per process,
`-XX:+UseG1GC`, `/tmp/ossl.args` (boringssl-static IN, dynamic tcnative OUT).
If a class in this suite aborts inside `gc/`, check that first rather than
reading it as a TLS defect.

Re-taken 2026-08-20 with `OpenSsl.isAvailable()` genuinely TRUE on every arm,
which no earlier column on this page can claim. HotSpot is run in the SAME batch
so a host effect cannot be read as a VM difference.

| class | HotSpot 25 | dev `86b13ed4c` | this branch |
|---|---|---|---|
| `JdkDelegatingPrivateKeyMethodTest` | 27 / 27 | 27 / 27 | **27 / 27** ✅ |
| `SslContextBuilderTest` | 21 / 21 | 21 / 21 | **21 / 21** ✅ |
| `SslHandlerTest` | 53 ok + 1 aborted | 53 ok + 1 aborted | **53 ok + 1 aborted** ✅ |
| `CloseNotifyTest` | 4 / 4 | 4 / 4 | **4 / 4** ✅ |
| `OpenSslPrivateKeyMethodTest` | ok=4 **failed=20** | 24 / 24 | **24 / 24** |
| `SniClientTest` | ok=4 **failed=23** | 27 / 27 | **27 / 27** |
| `ParameterizedSslHandlerTest` | 63 / 63 in 3.4 s | **6 stalls / 8 runs** | **1 stall / 8 runs**, and a different test — §D |
| `SslErrorTest` | **72 / 72** | ok=60 **failed=12** | ok=60 **failed=12** — filed, see below |
| `JdkSslEngineTest` | 755 ok + 66 aborted, 97 s | **900 s cap, no result** | **900 s cap, no result** |

Three rows need reading rather than counting:

* `OpenSslPrivateKeyMethodTest` and `SniClientTest` fail on HOTSPOT and pass on
  CratonVM. That direction is netty-tcnative's own behaviour on this host, not a
  CratonVM win; the numbers are recorded so the next reader does not mistake the
  gap for progress.
* **`SslErrorTest` is a genuine CratonVM defect that this page's own environment
  problem had been hiding**, and it is the sharpest possible illustration of it.
  The consolidated not-a-CratonVM-bug table carried this class as "identical
  `found=0 started=0` on both VMs" — a sound cross-check whose conclusion was
  wrong, because `found=0` on both VMs is not agreement, it is two VMs running
  nothing. With the classpath corrected the class generates 72 tests and
  CratonVM fails 12 of them, all `clientProvider = JDK` client-side rejections,
  all answering `TLSV1_ALERT_ACCESS_DENIED` where a certificate alert is
  required. Filed as
  `fixed-suite-bugs/netty/ssl-client-sends-access-denied-for-every-trust-rejection-FIXED-20260821.md`.
* `JdkSslEngineTest` now exceeds a 900 s cap on BOTH CratonVM binaries where
  HotSpot takes 97 s. Both arms, so it is not this branch — but the 755/821 this
  page's older columns record was measured on a classpath that generated far
  fewer tests, so it is not comparable either. It is a wall-clock wall, not a
  correctness result, and it is the reason this table has no ok/failed number
  for it.


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

## ~~D~~ — CLOSED 2026-08-20. The stall was a recursive read on the selector registry

For a week this section could say only that `ParameterizedSslHandlerTest`
**stops** — no failure, no exception, no thread that looks wrong — on both arms
of every A/B, at `reentryOnHandshakeCompleteNioChannel`, and that load changes
the rate but not the existence. It is a three-way deadlock on the process-wide
selector registry, and `gdb -p` on a stuck process names it in one dump.

### The instrument, and why the earlier ones could not see it

`--stack-dump-on-timeout=600` inside the 900 s cap, so a stalled run dumps
before `timeout` kills it blind. Its verdict on this hang:

```
=== T19.H1 watchdog: 0 thread(s) dumped; aborting process ===
=== no Java thread reached an interpreter dispatch point. That is EITHER
    JIT-compiled code … OR native (Rust) code. …
    Linux: `gdb -p <pid>` then `thread apply all bt` ===
```

Zero Java frames is itself the finding: every thread is in Rust. The
native-call ring it prints alongside narrows it to two threads that entered
their natives at the same millisecond and never left:

```
[43] tid=1    STILL-IN-NATIVE(536287ms ago) java/nio/channels/Selector.wakeup()
[63] tid=1457 STILL-IN-NATIVE(536286ms ago) java/nio/channels/Selector.selectNow()
```

`sudo gdb -p` on the live process, 26 threads, is the whole story:

```
Thread 6   sk_cancel_public  :3491 read HELD -> selector_cancel :1041 read -> PARKED
Thread 3   selector_open     :614  RwLock::write -> wait_for_readers      -> PARKED
20 others  open_flag :2294 / kernel_select_linux :1339 -> read            -> PARKED
```

### The defect

`parking_lot::RwLock::read()` is deliberately **not** recursive-safe: it blocks
whenever a writer is queued, so that writers cannot starve. `sk_cancel_public`
held the registry read guard across its search loop and called
`selector_cancel`, which takes the same lock. So a thread that already holds a
read guard parked behind the queued writer — and that writer is in
`wait_for_readers`, waiting for the guard the parked thread is still holding.
Nothing breaks the cycle, and every selector operation in the VM queues behind
it.

The trigger is a `SelectionKey.cancel()` racing a `Selector.open()`. netty
cancels keys on channel close and opens a selector per event loop, and this test
builds and shuts down a `MultiThreadIoEventLoopGroup` four times per invocation
— so the race is routine but not certain. That is exactly why:

* it is intermittent, and why load changes the RATE rather than the existence
  (this page retracted "load-only" twice before giving up on it; the batch below
  contains a stall at load **0.34**, the quietest run in it);
* it stalls on BOTH arms of any A/B whose two binaries share the registry, which
  is every A/B this page ever ran;
* it does not FAIL. HotSpot has no such lock and runs the same 63 tests in
  **3.4 s**.

`selectors_read()` replaces all 43 read sites: the FIRST acquisition on a thread
takes the fair `read()`, so writers still cannot starve; a NESTED one takes
`read_recursive()`, which does not queue behind a waiting writer and — since a
writer cannot hold the lock while this thread holds a read guard — cannot block
at all. The depth counter is a thread-local `Cell`, live in RELEASE as well as
debug: a `#[cfg(debug_assertions)]` check would have said nothing about the
binary these suites run.

Two call sites were also un-nested rather than merely made survivable:
`sk_cancel_public`, and the `epoll_wait` error path, where the guard was created
in an `if` CONDITION — which in Rust lives until the end of the whole `if`
STATEMENT, so it was still held inside the body that calls
`finish_in_flight_linux_select`.

### The A/B

Same protocol as the control batch, ABBA within each round, one batch so both
arms see the same host: `/tmp/ossl.args` (OpenSSL genuinely available — see the
environment note), `PerTestProgressRunner` so a killed run names the test that
was in flight, `-Djunit.jupiter.execution.timeout.mode=disabled`,
`--stack-dump-on-timeout=600`, 900 s cap, load recorded per run.

```
                                                          in flight when killed
r=0 base  load=4.99  rc=0    wall=95s   begin=63 end=63   -
r=0 fix   load=6.98  rc=0    wall=102s  begin=63 end=63   -
r=0 fix   load=6.14  rc=134  wall=601s  begin=18 end=17   testCloseNotifyNotWaitForResponse #9
r=0 base  load=2.91  rc=134  wall=605s  begin=23 end=22   reentryOnHandshakeCompleteNioChannel #5
r=1 base  load=1.53  rc=134  wall=604s  begin=19 end=18   reentryOnHandshakeCompleteNioChannel #1
r=1 fix   load=0.15  rc=0    wall=65s   begin=63 end=63   -
r=1 fix   load=1.43  rc=0    wall=64s   begin=63 end=63   -
r=1 base  load=1.48  rc=134  wall=605s  begin=25 end=24   reentryOnHandshakeCompleteNioChannel #7
r=2 base  load=0.04  rc=134  wall=604s  begin=23 end=22   reentryOnHandshakeCompleteNioChannel #5
r=2 fix   load=0.00  rc=0    wall=64s   begin=63 end=63   -
r=2 fix   load=1.01  rc=0    wall=66s   begin=63 end=63   -
r=2 base  load=1.42  rc=134  wall=604s  begin=22 end=21   reentryOnHandshakeCompleteNioChannel #4
r=3 base  load=0.02  rc=134  wall=604s  begin=23 end=22   reentryOnHandshakeCompleteNioChannel #5
r=3 fix   load=0.00  rc=0    wall=71s   begin=63 end=63   -
r=3 fix   load=0.99  rc=0    wall=65s   begin=63 end=63   -
r=3 base  load=1.23  rc=0    wall=71s   begin=63 end=63   -

  base (dev 86b13ed4c)   6 stalls / 8 runs,  ALL at reentryOnHandshakeCompleteNioChannel
  fix  (this branch)     1 stall  / 8 runs,  at a DIFFERENT test — see below
```

`rc=134` is the watchdog's own abort at 600 s, inside the 900 s cap;
`begin = end + 1` means exactly one test was in flight when it fired.

A control batch run first, before the fix existed, put two binaries that differ
by an unrelated TLS change at **7 stalls / 12 runs**, every one of them at
`reentryOnHandshakeCompleteNioChannel` — which is what "stalls on BOTH arms"
always meant: both arms had the deadlock.

**The load story ends here.** Four of the six control stalls above are at load
**0.02, 0.04, 1.42, 1.48** on an eight-core host that was otherwise idle, and
`fix` passed at 0.00 twice in the same batch. This page retracted "load-only"
twice and then said load "changes the RATE, not the existence"; the honest
statement is simpler — the deadlock needs a `SelectionKey.cancel()` to overlap a
`Selector.open()`, and load only changes how often two threads overlap.

The passing runs also got FASTER: 64–71 s on `fix` against 71–95 s on `base`,
because the fair-lock queueing this removes was costing every selector operation
in the process, not only the ones that deadlocked.


### What is left, and it is not this

The one `fix` stall is **not this defect**, and the VM's own watchdog separates
them without further work:

| | the deadlock (6/8 on `base`) | what is left (1/8 on `fix`) |
|---|---|---|
| test in flight | `reentryOnHandshakeCompleteNioChannel` | `testCloseNotifyNotWaitForResponse` |
| `--stack-dump-on-timeout` | **0 thread(s) dumped** | **1 thread dumped**, with Java frames |
| native-call ring | two threads `STILL-IN-NATIVE(536s)` | **no thread still in a native** |
| event loops | parked in Rust on a futex | parked in `NioIoHandler.select`, i.e. working |

One Java thread waiting on a `DefaultChannelPromise` while five event loops sit
in `NioIoHandler.select` is a completion that never arrives, not a lock. It is
filed as
`fixed-suite-bugs/netty/parameterizedsslhandlertest-object-wait-lost-a-delivered-notify-FIXED-20260824.md`,
with what it would take to close it.

**The second observation has since arrived, and it moved that page's
diagnosis.** A 20-run loop stalled once more — 2 in 28 overall — inside a
DIFFERENT test method, and resolving both BCIs with `javap -c -l` shows the two
waits are not the same thing: `testCloseNotify@230` is the client's CONNECT
future, not the close path the page was originally named for, and
`testAlertProducedAndSend@233` is an alert-delivery promise. The method name in
the progress line had been read as the diagnosis; it only names the test that
was running.

Note what this means for the section's original subject. §D was written as a
KEY-MATERIAL page: `NO_CERTIFICATE_SET`, "Unable to find key material for auth
method(s)", "PrivateKey type not supported PKCS#8". **None of those three
appears in any of the sixteen runs above, on either arm.** They were fixed by
the array-rooting sweeps recorded below; what remained after them was never a
key-material problem at all, which is why the sweeps kept not closing it.


### No regression, and the one class that looked like one

The lock change is not scoped to TLS: it rewrites how EVERY selector operation
in the VM takes the process-wide registry, so a green `ParameterizedSslHandlerTest`
is not evidence that nothing else moved. Same class selection the
`foreign-nio-subclass` fix used, and for the same reason — everything under
`channel.nio` / `channel.socket` / `test.udt` / `resolver.dns`, plus every class
whose NAME contains Socket / Server / EventLoop / Selector. **142 classes**, one
fork per class, both binaries, per-class `@@RESULT` diffed:

```
                PASS  FAIL  NOTESTS  ABORTED  HANG
base (dev)        88    44        8        2     0
fix (branch)      87    44        8        2     1
```

The 44 FAILs are the same 44, byte-identical. The whole diff is one line, and it
is a timeout rather than a failure: `AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest`,
`PASS 128/128 in 255 s` on base against the sweep's 300 s cap, `HANG` on the
branch.

That class was then re-run on its own, interleaved ABBA, 3 rounds, 700 s cap:

```
i=1 base 260s   fix 270s   fix 250s   base 253s
i=2 base 254s   fix 250s   fix 253s   base 333s   <- base, over the sweep's cap
i=3 base 273s   fix 247s   fix 272s   base 260s
```

**12 of 12 runs pass 128/128, on both arms.** The class simply takes 247–333 s,
and one BASE run took 333 s — above the same 300 s cap that produced the
"HANG". It was the cap, not the change; the sweep's cap is too tight for this
class and the diff would have gone the other way just as easily.

Worth keeping as a shape: a sweep-level `HANG` on a class whose passing time
sits within ~15% of the cap is a coin toss, not a result. The re-run that settles
it has to be the SAME binary, several times, not the two arms once each.


### The environment: this host could not run OpenSSL, for either VM

**Still true on dev today**, and it is now a command rather than a paragraph to
re-read. Measured 2026-08-20, HotSpot 25, same host:

```
java @common.args OpenSslAvailabilityProbe
  OpenSsl.isAvailable = false
  … UnsatisfiedLinkError: no netty_tcnative_linux_x86_64 in java.library.path

java @<generated> OpenSslAvailabilityProbe
  OpenSsl.isAvailable = true
  OpenSsl.versionString = BoringSSL
```

`netty-tcnative-2.0.81.Final-linux-x86_64.jar` — the DYNAMIC artifact the netty
Maven reactor resolves — bundles a `.so` that needs `OPENSSL_3.2.0` (`objdump
-p` names the version tag) and this host's `libssl.so.3` is **3.0.13**. The fix
has two halves and only the first is obvious:

1. put `netty-tcnative-boringssl-static-<ver>-<os>.jar` on the classpath — it
   links BoringSSL statically, so the host's OpenSSL version stops mattering;
2. **remove the dynamic `netty-tcnative-<ver>-<os>.jar`.** With both present
   netty finds the dynamic one and `OpenSsl.isAvailable()` stays false
   regardless of their order.

`apps/netty-suite-runner/gen-openssl-args.sh` derives that argfile from
`common.args` itself, so it cannot drift from the reactor, and refuses rather
than producing a quiet dud when the boringssl artifact is missing OR is a stub
— the `2.0.78` copy in this host's local repo has an EMPTY `META-INF/native/`,
which loads and then finds no library, i.e. it fails in the exact shape of the
problem the script exists to route around.

With `isAvailable()` false the classes that exist to test OpenSSL read as clean
PASSES while running a fraction of their tests. Measured before it was fixed,
`--gc g1`, ABBA over two binaries, all four arms byte-identical:

| class | found | started | ok | failed | aborted |
|---|---:|---:|---:|---:|---:|
| `ParameterizedSslHandlerTest` | **7** | 7 | 7 | 0 | 0 |
| `JdkDelegatingPrivateKeyMethodTest` | 2 | **0** | 0 | 0 | 0 (2 skipped) |
| `SslHandlerTest` | 54 | 54 | 37 | 15 | 2 |
| `SslContextBuilderTest` | 21 | 21 | 9 | 9 | 3 |
| `SniClientTest` | 3 | 3 | 3 | 0 | 0 |

`ParameterizedSslHandlerTest` at **7 of 63** is a PASS. Every one of
`SslHandlerTest`'s 15 failures and `SslContextBuilderTest`'s 9 is an `*OpenSsl*`
test whose cause chain ends in `UnsatisfiedLinkError`. Print
`OpenSsl.isAvailable()` before trusting any result from these classes;
`probes/OpenSslAvailabilityProbe.java` is that check and prints the
unavailability cause when the answer is no.

### What the array-rooting sweeps contributed, and what they did not

Two sweeps were run against this section before the deadlock was found, on the
theory that the stall was a rooting defect. **Neither closed it**, and the page
said so at the time — correctly. They did fix real defects of a real shape, and
those stand:

* `KeyStore.getCertificateChain`, `KeyStore.aliases`,
  `SSLSession.getPeerCertificates` and `getLocalCertificates` all built a
  reference array and then filled it in a loop whose body ALLOCATES, holding the
  array raw across those allocations. What the live array kept was whatever the
  collector left there: usually `null` (32 of them — the system trust-anchor
  count), occasionally a `DerValue` from the certificate parsing the loop had
  just done, which is why one site produced two unrelated-looking errors
  (`IllegalArgumentException: Null element in chain: [null × 32]` and
  `NoSuchMethodError: sun.security.util.DerValue.getEncoded()`).
* Six more of the same shape, two of them squarely on the OPENSSL key-material
  path: `x509_manager::materialize_string_array` (the whole of
  `X509KeyManager.getServerAliases` / `getClientAliases` — a holed alias list is
  what netty's `OpenSslKeyMaterialProvider` turns into `NO_CERTIFICATE_SET`),
  `kmf_engine_get_key_managers` / `tmf_engine_get_trust_managers` at length 1 (a
  one-element array is not exempt: the relocation moves the array, not the
  element count), and three cipher-suite / protocol name arrays in `t27_tls`.

`probes/KeyManagerAliasArrayRooting.java` reports `holes=0 shortfalls=0` on the
UNFIXED binary too, so it pins the contract going forward and is **not** a
reproduction. The honest reading is the one the page already had: those were
real defects worth fixing, and none of them was this one.

**The methodological cost is worth recording.** Two sweeps, ten converted sites,
and a probe that could not fail were spent on a theory the page's own A/B had
already refused twice ("an array-rooting sweep did NOT close it"; "§D does not
close, and this branch's rooting fixes are not its cure"). The instrument that
did close it — a native backtrace of a stuck process — was named in the VM's own
watchdog output the whole time, and needed `sudo`, which this host has.

## Repro

The classpath first, always. Everything else on this page is unreadable
without it:

```bash
cd apps/netty-suite-runner
./gen-openssl-args.sh -o /tmp/ossl.args
java @/tmp/ossl.args OpenSslAvailabilityProbe        # expect: isAvailable = true
```

Then the class, in the per-test form — the only one that names WHICH test a
killed run was inside, because the symptom is a stall rather than a failure:

```bash
<cratonvm> --java-home <jdk> --Xmx 1500m \
    --stack-dump-on-timeout=600 \
    @/tmp/ossl.args -XX:+UseG1GC \
    -Djunit.jupiter.execution.timeout.mode=disabled \
    PerTestProgressRunner io.netty.handler.ssl.ParameterizedSslHandlerTest
```

`--stack-dump-on-timeout` INSIDE the harness cap, not outside it: a run killed
by `timeout` alone dies blind. When the dump says "0 thread(s) dumped … no Java
thread reached an interpreter dispatch point", that is the finding, and the
next instrument is the one it names:

```bash
sudo gdb -p <pid> -batch -ex 'set pagination off' -ex 'thread apply all bt'
```

This host has passwordless `sudo`; an earlier note on this page said `gdb -p`
"needs sudo here" and stopped there, which is how §D stayed open for a week.

The whole-suite and rooting-guard forms:

```bash
CV_BIN=bin/<binary> bash run-netty-suite.sh --list /tmp/km.txt --gc g1 --shards 1 --timeout 600 --out /tmp/repro
<cratonvm> --java-home <jdk> @/tmp/ossl.args -XX:+UseG1GC KeyManagerAliasArrayRooting 400
```

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
- the retired `sslcontext-natives-ignore-a-third-party-spi` write-up — closed
  in the same branch; it is the reason `gen-openssl-args.sh` grew a `--bc18`
  mode.
- `fixed-suite-bugs/netty/parameterizedsslhandlertest-object-wait-lost-a-delivered-notify-FIXED-20260824.md`
  — what is left of §D once the deadlock is gone, and a much narrower thing.
