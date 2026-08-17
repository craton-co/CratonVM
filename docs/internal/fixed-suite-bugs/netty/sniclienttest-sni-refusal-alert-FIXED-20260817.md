# FIXED — `SniClientTest` 24/27 → 27/27: an SNI refusal queued its alert onto a connection that did not exist yet

**Status:** ✅ CLOSED 2026-08-17 on `fix/netty-sni-ocsp-rld-residuals-20260817`.
Retires the `SniClientTest` half of
`docs/known-issues/netty/sniclienttest-ocspclienttest-triage-20260816.md`; the
`OcspClientTest` half is retired separately in
`ocspclienttest-is-thirteen-rsa-keygens-CLOSED-20260817.md`.

Windows host, `cratonvm.exe` release build, one class per process
(`--shards 1`), both collectors.

| arm | G1 | ZGC |
|---|---|---|
| before | 24/27, 3 failed | 24/27, 3 failed |
| after | **27/27, 0 failed** | **27/27, 0 failed** |

## The defect

All three failures were `testSniSNIMatcherDoesNotMatchClient` at the
parameterizations netty labels `clientSslProvider = JDK`. That label is
misleading: netty's own test calls
`SniClientJava8TestUtil.testSniClient(serverProvider, clientProvider, false)`
against a helper declared `testSniClient(sslClientProvider, sslServerProvider,
match)`, so the two are swapped and the failing arm is the one whose **server**
context is the JDK provider — which is the side the test installs the
`SNIMatcher` on.

The gate itself worked. `engine_run_sni_match_check` consulted the matcher,
saw the refusal, and raised `SSLHandshakeException`. What it could not do was
tell the peer, because it queued the fatal alert like this:

```rust
with_engine(engine_id, |s| {
    if let Some(c) = s.conn.as_mut() {
        c.queue_fatal_alert(rustls::AlertDescription::UnrecognisedName);
    }
});
```

On the path that matters `conn` is `None`. The gate deliberately runs at
ClientHello time — `peek_client_hello_sni`'s doc explains why it cannot wait
for rustls to parse the record — and that is the call **before**
`engine_begin_or_defer` realizes the rustls connection. So the `if let` never
matched, no alert was ever produced, and the client learned only that the
channel had closed:

```
org.opentest4j.AssertionFailedError: Unexpected exception type thrown,
  expected: <javax.net.ssl.SSLException> but was:
  <io.netty.channel.StacklessClosedChannelException>
    at io.netty.handler.ssl.SniClientTest.testSniSNIMatcherDoesNotMatchClient(SniClientTest.java:83)
Caused by: io.netty.channel.StacklessClosedChannelException
    at io.netty.handler.ssl.SslHandler.channelInactive(SslHandler.java:1175)
```

`SslHandler.channelInactive` in the cause chain is the whole diagnosis: the
client's handshake future was completed by the channel going away, not by
anything the server said.

This is the same defect family as `03a3e2d20` ("a wrap that raises cannot also
deliver the alert it drained"), recorded for `SslHandlerTest`'s cipher-mismatch
scenario in `ssl-cert-validation-residuals-FIXED-20260813` /
`openssl-key-material-and-engine-residuals-20260813`. The triage page guessed
correctly that it would be the same family, and also correctly that the earlier
two-line fix would not cover it: this call path does not reach `do_wrap` with a
queued alert at all, because nothing ever queued one.

## The fix

Two halves, in `native-builtins/src/t27_tls.rs`:

* **Produce the alert without a connection.** With no rustls connection there
  is no record layer either — which is fine, because a pre-keys alert goes out
  as TLS plaintext, exactly as JSSE's own `ServerHandshakeContext` sends it
  when it refuses a hello before a ServerHello exists. Seven bytes into
  `EngineState::outbound`: `alert(21)`, legacy record version `0x0303`,
  length 2, level `fatal(2)`, description `unrecognized_name(112)`. `do_wrap`
  drains `outbound` independently of `engine_wrap_pump`, so the record reaches
  the caller even on a connection-less engine.
* **Do not raise from the unwrap.** `engine_run_sni_match_check` now answers
  "refused" instead of throwing, arms
  `EngineState::deferred_handshake_error`, and `do_unwrap` returns ORDINARY
  PROGRESS: `SR_OK`, `NEED_WRAP`, `bytesConsumed` = the ClientHello record.
  Consuming the record is what makes it progress —
  `SslHandler.decodeJdkCompatible` hands `unwrap` exactly one TLS record and
  treats `bytesConsumed != packetLength` as `NotSslRecordException`, and a call
  that consumed nothing reports `BUFFER_UNDERFLOW`, whereupon the caller waits
  for network data that is never coming and the deferred failure is never
  drained. The bytes are dropped rather than fed to rustls: a refused hello
  must not produce a ServerHello.

`handshake_status_of` already answers `NEED_WRAP` while a deferred failure is
pending (that was the second half of `03a3e2d20`), so the sequence is: unwrap
reports progress → wrap emits the alert (`produced=7`, still `NEED_WRAP`) →
wrap produces nothing and raises. The server gets its `SSLHandshakeException`,
the client gets the alert and raises its own `SSLException`, and the test's
`assertThrows(SSLException.class, …)` is satisfied on both sides.

## Verification

Isolated, one class per process:

| run | result |
|---|---|
| control G1 | `FAIL 27 found / 24 ok / 3 failed` |
| fixed G1 | `PASS 27 found / 27 ok / 0 failed` |
| control ZGC | `FAIL 27 / 24 / 3` |
| fixed ZGC | `FAIL 27 / 26 / 1` — see below |

The one remaining ZGC failure is **not** this defect. Re-run through the same
harness with `-Djunit.jupiter.execution.timeout.mode=disabled` (the only way
to lift a method-level `@Timeout`), which reports per-method wall time:

```
control ZGC:  3 FAILED  (parameterizations 1, 4, 7 — the three JDK-server arms)
fixed   ZGC:  0 FAILED, all 27 SUCCESSFUL
              first parameterization 21.4s, every later one 0.4-1.9s
```

So under ZGC the first parameterization pays 21 s of cold start (class
loading, TLS, certificate generation) against the method's own
`@Timeout(30000)`, and on a shared box that budget is what runs out. The
correctness defect is gone on both collectors; what is left on the ZGC arm is
the first-test start-up cost, which belongs to the throughput rows, not here.

Wide gate for the engine change (`handshake_status_of` and `do_unwrap` are on
every caller's path), G1, isolated:

| class | control | fixed |
|---|---|---|
| `SslHandlerTest` | 51/54, 50/54 | 50/54, 50/54 |
| `SniHandlerTest` | 32/32 | 32/32 |
| `SslContextBuilderTest` | 21/21 (72.2s) | 21/21 (28.2s) |
| `PemEncodedTest` | 1 ok / 2 assumption-skips | 1 ok / 2 assumption-skips |
| `ParameterizedSslHandlerTest` | HANG | HANG (pre-existing) |

`SslHandlerTest` is 50±1 on both arms — that ±1 flake is pre-existing and
independent (the two runs failed on different tests: a
`NoSuchMethodError: Object.checkServerTrusted` on one control run, a peer-reset
`IOException` on one fixed run).

Post-merge with `origin/dev` `eb3749f83` (which had landed its own substantial
`t27_tls.rs` work in `fix/netty-nio-pcap-tls-residuals-20260817`), re-verified
through the same instrument: **27/27 SUCCESSFUL on G1 and on ZGC**, and through
the suite runner `PASS 27 found / 27 ok / 0 failed`.

Rust gates on the merged tree: `cargo test -p cratonvm-gc --lib` 1619 pass /
0 fail; `cargo test -p cratonvm-native-builtins --lib` 3587 pass / 0 fail, and
3762 / 0 with `--features synthetic-jdk`; `cargo clippy -p cratonvm-gc
-p cratonvm-native-builtins` clean.

## Related

- `ssl-cert-validation-residuals-FIXED-20260813.md` — the first half of this
  defect family.
- `ocspclienttest-is-thirteen-rsa-keygens-CLOSED-20260817.md` — the other class
  the retired triage page covered.
- `resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817.md` —
  the sibling page retired in the same pass, and the G1 allocation-trigger fix
  that came out of it.
