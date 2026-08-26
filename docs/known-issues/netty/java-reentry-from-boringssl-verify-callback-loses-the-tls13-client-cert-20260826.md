# Java re-entering tcnative from inside BoringSSL's verify callback loses the TLSv1.3 client certificate

## Status

**OPEN, 2026-08-26 — found by regressing it, then worked around rather than
fixed.** No test in the suite fails on this today. It is written down because
the workaround is the only thing keeping it quiet, and the next person to add a
Java call on that path will hit it again with no idea why.

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

**The workaround is not the fix.** It removes this VM's exposure at one call
site. Any other Java that ends up on that callback — a future trust manager, a
key manager, a logging hook — re-introduces it.

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
