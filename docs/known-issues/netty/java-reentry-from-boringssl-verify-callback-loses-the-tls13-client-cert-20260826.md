# Java re-entering tcnative from inside BoringSSL's verify callback loses the TLSv1.3 client certificate

## Status

**OPEN, 2026-08-26.** Found by regressing it, worked around at the one call site
that caused the regression — and then measured to be **already present on a
second, independent path that the workaround does not touch**:
`OpenSslEngineTest.mustCallResumeTrustedOnSessionResumption` fails 12 of 48 on
`dev`, deterministically, with an identical failing index set before and after
the fix. So this is a live defect with a standing witness, not a hazard that only
exists if someone reintroduces it.

## What happens

With netty's OpenSSL provider configured `setUseTasks(false)`, BoringSSL runs
netty's certificate callbacks **synchronously, re-entrantly, inside the tcnative
`SSL_do_handshake` native**. If Java code called from that callback re-enters
tcnative on the same `SSL*` — even for a read-only query — the client's TLSv1.3
`Certificate` flight is never sent, and the server ends the handshake with

```
error:100000c0:SSL routines:OPENSSL_internal:PEER_DID_NOT_RETURN_A_CERTIFICATE
```

TLSv1.2 is unaffected. `useTasks=true` is unaffected. **HotSpot is unaffected**:
it makes the same re-entrant calls and completes the handshake.

## How it was found, and the measurement that pins it

`x509_manager::check_server_trusted_extended` was added on 2026-08-26 to close a
real hole (the extended `X509TrustManagerImpl` overloads were skipping hostname
verification entirely —
`fixed-suite-bugs/netty/ssl-parameterized-classes-exceed-180s-timeout-masking-real-failures-20260826.md`).
Its first line read the endpoint identification algorithm with
`engine.getSSLParameters()`. On netty's `ReferenceCountedOpenSslEngine` that
method is `synchronized` and calls `SSL.getOptions(ssl)` and, through
`super.getSSLParameters()` -> `getEnabledCipherSuites()`, `SSL.getCiphers(ssl)`
— tcnative natives, on the very `SSL*` BoringSSL is currently inside.

`probes/OpenSslTls13ClientCertProbe.java` is that reduced to eight rows: a real
mutual-TLS handshake with a client certificate, over protocol × `useTasks` ×
transport. One run each, same host, same argfile:

| binary | result |
| --- | --- |
| HotSpot 25 | **8/8 PASS** |
| CratonVM, before the trust-manager change | **8/8 PASS** |
| CratonVM, with `getSSLParameters()` in the callback | **2 FAIL** |

and the two failures are exactly

```text
@@ROW FAIL transport=::1       protocol=TLSv1.3 useTasks=false … serverCause=…PEER_DID_NOT_RETURN_A_CERTIFICATE
@@ROW FAIL transport=127.0.0.1 protocol=TLSv1.3 useTasks=false … serverCause=…PEER_DID_NOT_RETURN_A_CERTIFICATE
```

Both transports fail, which is what rules out the IPv6 loopback — an earlier
reading of this blamed the transport, because the only netty test that changed
behaviour when the wildcard bind became dual-stack
(`testMutualAuthSameCertChain`, which connects to `serverChannel.localAddress()`
verbatim) moved from an IPv4 to an IPv6 connection at the same time. It is not
the transport. **The probe binds each loopback address explicitly so that axis
is the probe's choice and not a side effect of what a wildcard bind resolves to.**

In every affected test the identification algorithm is explicitly `null`
(`SslContextBuilder…endpointIdentificationAlgorithm(null)`), so the entire extra
cost was ONE call that discovered there was nothing to do. That is what makes
the attribution tight: no identification logic ran, no exception was raised,
nothing was rejected — a read-only query from the wrong place was enough.

`testClientHostnameValidationFail` passes throughout, on the same
`useTasks=false` parameterisations, and that is not a contradiction: its client
presents no certificate, so there is no client `Certificate` flight to lose.

## The standing witness: `mustCallResumeTrustedOnSessionResumption`

This one is NOT the regression, and that is what makes it worth having. It
installs its own `SessionValueSettingTrustManager` — an application
`X509ExtendedTrustManager` that does real work (it writes a value into the
`SSLSession`) from inside the same callback — so it never reaches
`x509_manager`'s shim at all, and the field fast path below cannot help it.

Interleaved, one run per arm, same host, same argfile:

| arm | result | failing invocations |
| --- | --- | --- |
| HotSpot 25 | **48/48 ok**, 23.6 s | — |
| CratonVM, before the endpoint-identification fix | 36 ok, **12 failed**, 782 s | `10 12 14 16 26 28 30 32 42 44 46 48` |
| CratonVM, after it (post `dev` merge) | 36 ok, **12 failed**, 797 s | `10 12 14 16 26 28 30 32 42 44 46 48` |

**The same twelve, both arms.** Decoded against
`OpenSslEngineTestParam.expandCombinations` they are exactly `TLSv1.3` AND
`useTasks=false`, for all three buffer types and both `delegate` / `useTickets`
values — the identical signature to the regression above, arrived at from
completely different Java.

It is also, on its own, most of what this class costs on CratonVM. Measured over
a whole-class run: 2 618 s across 31 invocations of this method against 798 s
for the other 672 tests put together, because each failure burns a 60 s JUnit
timeout. Fixing it would take `OpenSslEngineTest` from roughly 2.4 h to about
1.5 h here.

## The workaround that is in the tree

`extended_tm_identification_algorithm` reads netty's
`endpointIdentificationAlgorithm` FIELD directly when the receiver is a
`ReferenceCountedOpenSslEngine`, and only falls back to `getSSLParameters()` for
engines that have no such field (the JDK's `SSLEngineImpl`, whose
`getSSLParameters()` is pure Java and re-enters nothing). A field read runs no
Java, allocates nothing and calls no native, so the callback is left exactly as
it was before the hostname fix — while a client that DOES ask for identification
still gets it.

`CRATONVM_X509_TM_NO_IDENTIFY_SERVER` / `..._CLIENT` disable each direction
outright. They exist so this attribution stays a one-run A/B rather than a
rebuild; they are diagnostic levers, and the SERVER one re-opens a real security
hole, so neither is a configuration.

**The workaround is not the fix**, and the section above is the proof rather
than the warning: an application trust manager doing its own work on that
callback fails today, on `dev`, with the workaround in place. The workaround
removes this VM's exposure at ONE call site. Any other Java that ends up on that
callback — a trust manager, a key manager, a logging hook — hits it.

## What is NOT established

* **Which re-entrant call breaks it.** `getSSLParameters()` makes at least two
  (`SSL.getOptions`, `SSL.getCiphers`) and takes the engine's monitor. Nothing
  here separates them, and the answer matters: a lock-ordering problem and a
  BoringSSL state-machine problem need opposite fixes.
* **Whether the loss is on the send side or the receive side.** The server says
  the peer sent nothing; nobody has looked at the wire. A capture, or
  `SSL_CTX_set_msg_callback`, would say whether the client emitted a
  `Certificate` message at all.
* **Whether this is the `ClassId(0)` / stale-receiver family.**
  `known-issues/netty/parameterizedsslhandlertest-residual-stalls-20260824.md`
  describes a different symptom on the same boundary (a `NoSuchMethodError`
  naming `java.lang.Object`). One such event WAS logged elsewhere in the same
  netty run, from `SingleThreadIoEventLoop.run`, so the family is live on this
  workload — but no such error accompanies these two rows, and a silent loss is
  not that signature. Assuming they are one thing would be assuming the answer.

## Next step

`OpenSslTls13ClientCertProbe` is a 30-second deterministic oracle, which is what
makes this cheap to bisect. Add one re-entrant call at a time back into the
callback — `SSL.getOptions` alone, then `SSL.getCiphers` alone, then the bare
`synchronized` block with no native inside — and see which row flips. Three
runs answer the first bullet above.

`CRATONVM_X509_TM_PARAMS_VIA_METHOD=1` re-arms the defect on a shipped binary
without editing anything, which is the control for each of those three runs.
And `mustCallResumeTrustedOnSessionResumption` is the arm to confirm a candidate
fix against, because it is the one this VM fails WITHOUT any help from
`x509_manager`.

## Repro

```bash
cd apps/netty-suite-runner
javac @cp-javac.args -d /tmp/cls ../../probes/OpenSslTls13ClientCertProbe.java
java @common.args OpenSslTls13ClientCertProbe            # HotSpot: 8/8
cratonvm --java-home <jdk25> --Xmx 1500m -XX:+UseG1GC \
  @common.args OpenSslTls13ClientCertProbe               # expect 8/8 with the workaround in place

# re-arm the defect without editing code:
CRATONVM_X509_TM_NO_IDENTIFY_SERVER=  cratonvm … OpenSslTls13ClientCertProbe   # still 8/8
```

## Related files

- `native-builtins/src/x509_manager.rs` — `extended_tm_identification_algorithm`
  (the field fast path), `check_server_trusted_extended`
- `types/src/flags.rs` — `x509_tm_no_identify_client` / `x509_tm_no_identify_server`
- `probes/OpenSslTls13ClientCertProbe.java`
- `apps/netty/handler/src/main/java/io/netty/handler/ssl/ReferenceCountedOpenSslEngine.java`
  — `getSSLParameters()`, and `OpenSslEngineTestParam.wrapContext`'s `setUseTasks`
