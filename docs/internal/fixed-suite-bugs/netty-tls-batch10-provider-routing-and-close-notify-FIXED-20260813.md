# FIXED — netty `handler.ssl` batch 10: JCA provider routing, and the alerts `wrap` never emitted

**Status:** ✅ FIXED 2026-08-13 on `fix/netty-tls-mxbean-20260812`. Replaces
`docs/known-issues/netty/tls-batch10-encrypted-keys-and-handshake-gaps-20260812.md`,
whose measurements stand but whose two main hypotheses did not.

Two root causes accounted for every cluster on that page. Neither was the one
the page proposed.

---

## Root cause 1 — CratonVM ignored an explicitly requested JCA `Provider`

The page split its encrypted-key failures into two clusters and proposed a
different fix for each: implement PBES1 for cluster 1, and diff the bytes for
cluster 2 ("HotSpot passes the same test on the same file, so the bytes
differ"). **The bytes are identical.** Measured on both VMs, from the same
files:

```
DEK-Info  = AES-128-CBC,F87E93E4D6D3AA4C23CF2593880E5D35
bodyLen   = 1200  bodyMd5 = 586d254ff13ebf526be55dd041773964     (identical)
BC.iv     = byte[16] md5=83431e9bd1e6619d502987dc9d10723d        (identical)
BC.keyBytes = byte[1200] md5=586d254ff13ebf526be55dd041773964    (identical)
```

What differs is **who decrypts them.** netty's `SslContext.toPrivateKey` tries
BouncyCastle *first* and only falls back to `PemReader` + the JDK's own
`EncryptedPrivateKeyInfo` when BC returns null. On HotSpot BC reads every one
of netty's encrypted test keys, so the JDK parser is never reached. On CratonVM
BC failed, netty fell back, and the JDK parser was handed a traditional PKCS#1
blob — hence `IOException: Invalid lenByte`, and hence
`NoSuchAlgorithmException: Cannot find any provider supporting
PBEWithMD5AndDES` from the fallback's own `SecretKeyFactory.getInstance`.
Neither message was about a missing algorithm. Both were about a provider that
never got to run.

BC failed for two independent reasons, both "the `Provider` argument was
validated and then discarded":

**(a) `SecretKeyFactory.getInstance(alg, Provider)` produced a silently wrong
key.** That overload is not registered as a native, so the real JDK bytecode
runs and correctly builds a factory around BouncyCastle's own SPI — and then
CratonVM's `generateSecret` / `getAlgorithm` / `getProvider` natives, registered
on `javax/crypto/SecretKeyFactory`, shadowed the real bytecode for it. With no
entry in this VM's own table, `generateSecret` fell out of its
`unwrap_or(256)` default and ran PBKDF2-HMAC-SHA256 under another algorithm's
name:

```
                              HotSpot 25                        CratonVM (before)
getInstance("PBKDF-OpenSSL", bc)   provider=BC alg=PBKDF-OpenSSL   provider=SunJCE alg=""
generateSecret(...).getEncoded()   90e508cc4fc9798bdec87516bebe5ecd   bcfb6da32cd0aae96fdc53ed8980f72b
```

A wrong key with no error anywhere — the worst shape this workspace has a name
for. BouncyCastle then reported `BadPaddingException: Given final block not
properly padded`, which reads like a wrong password.

*Fix:* the three natives now answer only for factories **this VM built**
(`skf_receiver_is_ours`, keyed on the table `getInstance` populates). For any
other receiver, `generateSecret` calls the real `spi.engineGenerateSecret`, and
`getAlgorithm`/`getProvider` read the real object's own fields — type-checked,
because an unchecked `get_field_by_name` handed a `String` back where a
`Provider` belonged and the caller died on `String.getName()`.

**(b) `Cipher.getInstance(alg, Provider)` used CratonVM's implementation
whatever provider was named.** For transformations this VM implements that is
invisible (SunJCE and BC agree on `AES/CBC/PKCS5Padding`). For everything else
it turned a working call into `NoSuchAlgorithmException` — including the
PKCS#12 PBE OIDs BouncyCastle asks its own provider for
(`Cipher.getInstance("1.2.840.113549.1.12.1.3", bcProvider)`), which is exactly
what `test_encrypted.pem` needs.

*Fix:* when this VM cannot serve the transformation **and** the named provider
owns the service, instantiate that provider's `CipherSpi` (through the same
`build_jca_impl` path `sun.security.jca.GetInstance` interception already uses)
and forward `init`/`update`/`doFinal`/`getOutputSize`/`getBlockSize`/`getIV`/
`getParameters` to it. Deliberately narrow: delegation is attempted **only**
where this VM would otherwise throw, so no currently-passing call changes
behaviour and a bug in it fails as "still throws", not as "silently computes
something else".

`Cipher.update([BII)[B` had to be registered as part of this — it was
unregistered, so it fell through to real `Cipher.update` bytecode and threw
`IllegalStateException: Cipher not initialized` (native `init` keeps its state
in `CIPHER_TABLE` and never writes the real object's SPI fields). It is the
overload every streaming caller reaches, including BouncyCastle's
`jcajce.io.CipherInputStream`.

### What this means beyond netty

Third-party JCE providers — BouncyCastle, Conscrypt, ACCP — could not supply a
`Cipher` or a `SecretKeyFactory` on CratonVM at all, and in the
`SecretKeyFactory` case the substitution was silent. Any library that installs
its own provider and asks for it by name was affected.

---

## Root cause 2 — `wrap` never emitted the alert `closeOutbound` had queued

The page's clusters 3 and 4a ("handshake completes but carries no data", and
`testAlertProducedAndSend` blocking forever in `awaitUninterruptibly`) are one
defect, and the page's reading of cluster 3 was slightly off: the assertion
that fails is `CloseNotifyTest.assertCloseNotify`, and `0` is not "no data
arrived" but **an empty outbound buffer where a close_notify record belongs**.

`do_wrap` opened with a closed-outbound short circuit:

```rust
let closed = with_engine(id, |s| s.closed_outbound).unwrap_or(false);
if closed {
    return alloc_engine_result(ctx, SR_CLOSED, HS_NOT_HANDSHAKING_R, 0, 0);
}
```

`closeOutbound()` queues a `close_notify` on the rustls connection. The wrap
that follows it is the call JSSE specifies as the one that *emits* that record.
This returned `CLOSED, produced=0` instead, and the alert — generated,
encrypted — sat in rustls's write queue forever.

*Fix:* `closed` no longer short-circuits. It suppresses reading application
data from `srcs` (JSSE consumes nothing after close) and forces the reported
status to `CLOSED`; the ordinary drain runs, so `bytesProduced` is the alert's
real length. netty writes `out` before it inspects the status, so `CLOSED` with
a non-zero `bytesProduced` is precisely what gets the record onto the wire and
then stops its wrap loop.

Three further gaps were on the same path:

* **A `TrustManager` rejection sent nothing.** rustls has already accepted the
  chain by the time the Java manager is consulted — the consultation is
  deliberately deferred until after `process_new_packets` so that calling into
  the JVM never happens while the engine registry lock is held — so rustls
  generates no alert of its own. `engine_run_trust_check` now queues a fatal
  `certificate_unknown` (`reject_peer_with_fatal_alert`), through a
  `queue_fatal_alert` added to the vendored rustls fork. Without it the peer
  saw an unexplained TCP close and could not tell a rejected certificate from
  a dropped network.
* **A received alert surfaced as a bare `IOException`.** netty's
  `testAlertProducedAndSend` waits for `cause.getCause() instanceof
  SSLException`. Post-handshake record-layer failures now throw
  `javax.net.ssl.SSLException` — which extends `IOException`, so every existing
  `catch (IOException)` is unaffected — and an `AlertReceived` carries JSSE's
  own wording, `Received fatal alert: <desc>`.
* **`unwrap` never reported `CLOSED`.** JSSE reports `Status.CLOSED` from the
  unwrap that consumes the peer's `close_notify` and from every one after it;
  netty's `SslHandler.unwrap` switches on exactly that to fire
  `SslCloseCompletionEvent`. `do_unwrap` now reports it (and sets
  `closed_inbound`, which is the same fact through `isInboundDone()`), reading
  a `has_received_close_notify` accessor added to the vendored fork.

---

## Measured

One JVM per class, same host, same jars. "control" is `origin/dev` unpatched;
"after" is this branch; "HotSpot" is stock JDK 25.

| class | HotSpot | control | after |
|---|---|---|---|
| `SniHandlerTest` | 18 ok | 11 ok / **7 failed** | **18 ok / 0 failed** |
| `CloseNotifyTest` | 2 ok / 2 aborted | 0 ok / **2 failed** | **2 ok / 0 failed** |
| `ParameterizedSslHandlerTest` | 7 ok, 10 s | **HANG** (>17 min) | **7 ok / 0 failed, 8 s** |
| `ApplicationProtocolNegotiationHandlerTest` | 8 ok | 6 ok / **2 failed** | **8 ok / 0 failed** |
| `JdkSslServerContextTest` | 35 ok / 1 aborted | 26 ok / **9 failed** | 33 ok / **2 failed** |
| `JdkSslClientContextTest` | 34 ok / 1 aborted | 24 ok / **10 failed** | 29 ok / **5 failed** |
| `SslContextBuilderTest` | 9 ok / 9 failed / 3 aborted | 6 ok / 12 failed | 6 ok / 12 failed |
| `SniClientTest` | 2 ok / 1 failed | 1 ok / 2 failed | 1 ok / 2 failed |

`SslContextBuilderTest`'s nine HotSpot failures are all
`UnsatisfiedLinkError: failed to load the required native library` —
netty-tcnative is absent on this host — and `SniClientTest`'s one is a port
collision. Subtract the environment before counting, as the page this replaces
says.

**34 CratonVM-specific failures plus one hang, down to 11 and none.** The four
classes that carried the page's clusters 1, 3 and 4a now match HotSpot exactly.

**No regressions:** every class the control run passed, the patched run passes.

### The last two steps of the close sequence

The `close_notify` drain alone got `CloseNotifyTest`'s TLS 1.3 parameterisation
and most of `ParameterizedSslHandlerTest`. Two further facts were needed, and
both are visible only under TLS 1.2:

* **Answer the peer's `close_notify` automatically.** RFC 5246 §7.2.1 requires
  the response; RFC 8446 §6.1 makes it optional and JSSE's TLS 1.3 engine does
  not send one — an asymmetry netty encodes directly (`CloseNotifyTest`'s
  `jdkTls13` branch asserts the automatic response on every *other*
  parameterisation). rustls queues nothing on its own either way.
* **A fully-closed engine can still owe the peer a record.**
  `handshake_status_of` opened with "both halves closed → NOT_HANDSHAKING",
  which is the answer that makes a caller stop wrapping — so the reply queued
  by the step above was never emitted. It now reports NEED_WRAP while rustls
  still `wants_write()`. That guard was invisible until something started
  queueing a record *at the moment of closing*, which is exactly what the
  automatic response does.

---

## Residuals — tracked separately, not part of this fix

Eleven failures survive, all **different defects** that this triage surfaced
rather than caused, plus `JdkSslEngineTest`. They are recorded in
[`docs/known-issues/netty/tls-batch10-residuals-20260813.md`](../../known-issues/netty/tls-batch10-residuals-20260813.md):
`TrustManagerFactorySpi` dispatch resolving against `KeyStore`, PBES2
`AlgorithmParameters` decoding, a caller-supplied `SecureRandom` being ignored,
combined cert+key PEM files, a `TrustManager` that throws an `Error`, and
`JdkSslEngineTest` not finishing.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.SniHandlerTest
```
