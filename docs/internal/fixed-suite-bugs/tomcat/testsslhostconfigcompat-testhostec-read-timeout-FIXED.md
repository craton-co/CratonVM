# `TestSSLHostConfigCompat.testHostEC[JSSE-KEYSTORE]` — intermittent 300 s stall — FIXED

**Status: FIXED 2026-08-01** on branch
`fix/tomcat-sslhostconfig-ec-timeout-20260801`.
Supersedes `docs/known-issues/tomcat/testsslhostconfigcompat-testhostec-read-timeout-20260801.md`.

**Two defects, not one.** The second was invisible until the first was fixed —
the window it lives in used to deadlock before it could be reached.

## One line each

1. **The 300 s stall.** `http_url_connection::perform`'s https branch parked the
   calling Java thread in a blocking `recv()` **without a GC blocking region**,
   so any stop-the-world cross-thread JIT takeover requested while it waited
   deadlocked against it — and the peer it was waiting for was another thread of
   the *same* VM, parked at that same barrier.
2. **`TrustManager rejected the peer certificate chain`.** `t27_tls::
   engine_run_trust_check` pinned the `X509Certificate[]` chain **after**
   filling it, so a moving collection triggered by the very allocations in the
   fill loop relocated the array and the remaining stores were silently
   dropped. Same test, ~1 run in 24, 397 ms instead of 300 s.

## What the open doc got right, and what it got wrong

Right: the flake is real, is CratonVM-only, and was newly *exposed* (not newly
caused) by the hostname-verification fix — pre-fix, the class never reached the
point where this could be observed.

Wrong: the "lead worth checking first" — `localhost-ec.jks` carrying `CN=localhost`
with no SubjectAltName, so `testHostEC` is the one variant taking the legacy
commonName-fallback branch of `x509_manager::verify_hostname`. That is true of
the certificate and irrelevant to the failure. Nothing about EC, about the
cipher suite, or about endpoint identification participates. `testHostEC[JSSE-KEYSTORE]`
is simply **test 12 of 78** — the point in this class's run where the young
generation fills and the run's Nth GC lands. Any test at that offset would do;
the certificate under it is a coincidence of ordering.

Also worth correcting for future readers: the open doc's symptom, a
`java.net.SocketTimeoutException: Read timed out` out of
`TomcatBaseTest.methodUrl:709`, is only one of the two faces of this defect. It
appears when the pause lands while the client is in `read_response`. When the
pause lands a few milliseconds earlier — while the client is still in the
handshake loop — the same 300 s wait surfaces instead as
`javax.net.ssl.SSLHandshakeException: handshake read: … (os error 10060)`.
Both reproduced here; both are the same wedge.

## Root cause 1 — the 300 s stall

`native-builtins/src/http_url_connection.rs::perform`, https branch. The whole
exchange — TLS handshake, request write, response read — ran inside an
`set_active_native_context` window and with **no**
`begin_blocking_region()`/`end_blocking_region()` bracket anywhere. The code
said why:

> A loopback exchange is fast, so never GC-parking for this whole branch (a GC
> during these few milliseconds simply waits for this thread, like any other
> ordinary native call) is the safe, low-risk trade-off

That premise fails precisely in the configuration every embedded-server test
uses, where **the peer is another thread in the same VM**:

1. The test thread calls `getResponseCode()`, lands in `perform`, and blocks in
   `recv()` waiting for the server's next TLS flight (or the HTTP response).
2. Some other thread reaches `maybe_gc` and starts a stop-the-world
   cross-thread JIT takeover. Every mutator is asked to reach the GC barrier;
   threads *in JIT code* can be forcibly taken over instead.
3. The test thread is in neither state: it is inside a Rust native, parked in a
   syscall, having never published itself as GC-blocked. It cannot reach a
   safepoint and cannot be taken over. `stw_take_over_and_wait` spins.
4. Every Tomcat thread — acceptor, poller, `http-nio-*-exec-*` — is parked at
   that barrier. The server can therefore never send the bytes the test thread
   is waiting for.
5. Nothing moves until the socket's `SO_RCVTIMEO` fires.
   `TomcatBaseTest.DEFAULT_CLIENT_TIMEOUT_MS` is `300_000`.

The stderr signature is exactly one line, then five minutes of silence:

```
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still
waiting for cooperative mutators rounds=64 pending=1 taken=0
```

`pending=1` is the test thread. This is the same family as the fixes named in
`stw_takeover_should_scan`'s own doc comment (`elinjsp-socket-read-timeout`,
`stw-crossthread-jit-takeover-hang-cluster`,
`wildfly-standalone-boot-stw-jit-takeover-hang`), and the **plain-HTTP branch
of this very same function** has carried the correct bracket, with this
rationale spelled out, since 2026-07-14. Only the https branch was exempted.

## Root cause 2 — the chain array, unpinned while being filled

With fix 1 in, 12 runs were clean and a 13th failed in **397 ms** with
`javax.net.ssl.SSLHandshakeException: TrustManager rejected the peer
certificate chain`, from the same `TomcatBaseTest.methodUrl:709` and the same
`testHostEC[JSSE-KEYSTORE]`.

`t27_tls::engine_run_trust_check` did this:

```rust
let arr = ctx.new_ref_array(ClassId::new(0), chain.len());
for (i, der) in chain.iter().enumerate() {
    let mirror = crate::keystore::make_x509_mirror(ctx, "peer", der); // ALLOCATES
    ctx.set_array_element(arr, i, Value::Object(Some(mirror)));       // stale `arr`
}
let auth_type_str = ctx.create_string(auth_type);                     // ALLOCATES
let base = ctx.pin_native_root(arr);                                  // ...pinned HERE
```

`make_x509_mirror` allocates a `byte[]` and runs the real
`sun.security.x509.X509CertImpl` constructor; `create_string` allocates as
well. Any of them can trigger a moving young collection that relocates `arr`,
and the store that follows is then **silently dropped** by the heap guard
(`gen_heap: … out-of-bounds … dropped` — the warning is in the logs of every
run, fixed or not). `X509TrustManagerImpl.checkServerTrusted` received a chain
with a null element and threw. Textbook Family-1: a native local held live
across an allocation.

This was not caused by fix 1 — the ordering bug is older — but it could not be
observed before it, because that same window is the one that used to deadlock.

`make_x509_mirror`'s own synthetic-mirror fallback has the identical shape
(both the `byte[]` and the mirror object are live across `create_string`) and
is fixed the same way, even though the real-`X509CertImpl` path is the one
actually taken here.

## Fix

`native-builtins/src/http_url_connection.rs` only.

1. **Handshake loop** — `read_tls` and `write_tls` each get their own
   `begin_blocking_region()` / `end_blocking_region_refs()` bracket.
   `process_new_packets()` stays **outside** every region: it is the one place
   rustls can call back into Java (`JavaKeyManagerResolver::resolve` →
   `KeyManager.chooseClientAlias`/`getPrivateKey`), and running bytecode while
   marked GC-parked is what `set_active_native_context`'s doc forbids. The
   earlier attempt that deadlocked the class-loading/vtable-install locks had
   put the region around `process_new_packets` itself — exactly backwards.
   Each `?` is taken *after* the region is closed, so no error path can leave
   the thread permanently marked blocked (the mirror-image failure the
   `SSLSocketOutputStream` drain loop already warns about).

2. **After the handshake** — the active-context window is dropped and the
   request write, `read_response`, and the post-handshake ticket drain run in
   one region, with the error carried out of an inner closure so
   `end_blocking_region()` always runs. Dropping the window is safe: the old
   comment held it open across `read_response` for a Tomcat-`SSLAuthenticator`
   mid-connection renegotiation, but rustls 0.23 refuses renegotiation on both
   sides — a post-handshake `HelloRequest` is answered with `no_renegotiation`
   and never processed (`rustls/src/common_state.rs::process_msg`;
   root-caused from the dependency's own source in
   `tls-ocsp-clientcert-validation-not-enforced-FIXED.md`, "Residual #2
   follow-up"). No bytecode can run there, so keeping the window open would
   only make the region unsound.

3. **Two `ObjectRef`s now span a GC point** and are re-read: `connection`
   through `end_blocking_region_refs` inside `perform`, and `this` through a
   pinned native root across `huc_real_perform`'s redirect loop (which also
   covers `huc_client_tls_restrictions`, since that allocates and runs
   bytecode between hops). Without these the fix would have traded a hang for
   a stale-native-local — the Family-1 shape.

Sibling blocking paths were audited and already carry the bracket, so nothing
else needed changing: `SSLSocketInputStream`/`OutputStream` read/write
(`phases_late/ssl_security.rs`), blocking `SocketChannel.read`
(`phases_late/net_channels.rs`), and `rustls_client_connect`'s callers
(`net_phase_e.rs`).

4. **`t27_tls::attach_trust_managers_to_ctx`** pins the managers before
   `capture_accepted_issuer_dns` and re-reads them through those pins before
   the table takes ownership; `capture_accepted_issuer_dns` re-reads each
   manager per iteration and pins the `getAcceptedIssuers()` array it walks.
   `attach_key_managers_to_ctx` has the same collect-then-insert shape but
   nothing allocating in between, so it is left alone.

5. Two more instances of the same Family-1 shape, found while chasing the
   above and fixed even though neither turned out to be the failure:
   **`engine_run_trust_check`** now pins the chain array before filling it
   rather than after, and **`keystore::make_x509_mirror`**'s synthetic-mirror
   fallback pins its `byte[]` and mirror object across `create_string`.

6. **Diagnostics.** `engine_run_trust_check` used to discard the thrown
   exception unless `CRATONVM_DBG=tls-auth` was set, and `perform` then
   flattened even that into a fixed string, so all three ways to reach
   "TrustManager rejected the peer certificate chain" — a genuine refusal, an
   `AbstractMethodError` from a bare-interface stub, and a VM fault underneath
   it — read identically. The exception's class and `getMessage()` now ride
   into the `SSLHandshakeException`, and out through `perform`'s
   `Result<_, String>` via a thread-local hand-off. **This is what solved
   defect 2**: the first failure after it landed said
   `java/lang/NoSuchMethodError: java.lang.Object.checkServerTrusted(…)`, and
   a receiver whose class is `java.lang.Object` is ClassId(0) — the
   reclaimed-slot signature — which turned a guessing game into a one-line
   diagnosis. The debug flag was no substitute: its `eprintln!`s perturbed the
   timing enough to hide the race entirely (0 failures in 10 runs with it on,
   against 2 in 24 with it off).

## Verification

All `TestSSLHostConfigCompat` (78 tests), Windows box, `--Xmx 2g`, the doc's own
repro environment. Binaries are uniquely named per stage; the "wedge" column is
a ~325 s run with the STW warning, the "reject" column a ~365 ms
`TrustManager rejected` on the same test.

| binary | what it has | runs | 300 s wedge | reject |
|---|---|---:|---:|---:|
| `cratonvm-tlsec-base` | neither fix (branch point) | 9 | **2** | 0 |
| `cratonvm-tlsec-fix1` | STW brackets | 12 | 0 | 0 |
| `cratonvm-tlsec-fix2` | + helper extraction | 12 | 0 | **1** |
| `cratonvm-tlsec-fix3` | + chain-array pin | 24 | 0 | **2** |
| `cratonvm-tlsec-fix4` | + rejection diagnostic | 30 | 0 | **1** |
| `cratonvm-tlsec-fix5` | + TrustManager pin | **80** | **0** | **0** |

The wedge is gone in 158 post-fix runs against 2 in 9 before it. The reject ran
at 4 in 66 (6.1 %) across fix2–fix4 and is 0 in 80 after the manager pin —
about a 0.6 % chance of seeing that if the rate were unchanged.

`CRATONVM_GC=-moving-young` on `fix4` (pre-manager-pin): **14/14 clean**. That
is the arm that localised defect 2 to a *relocation*, not a liveness, failure.

Neighbouring TLS classes, pre-fix (`cratonvm-tlsec-base`) vs post-fix
(`cratonvm-tlsec-fix5`) — same pass/fail everywhere, all a little faster:

| class | pre | post |
|---|---|---|
| `TestSsl` | 21, 1 fail, 386 s | 21, 1 fail, 312 s |
| `TestSSLHostConfigCipher` | 12/12, 10.4 s | 12/12, 6.5 s |
| `TestSslHandshakeFailure` | 1/1, 8.6 s | 1/1, 5.2 s |
| `TestCustomSslTrustManager` | 9/9, 11.9 s | 9/9, 7.4 s |
| `TestClientCert` | 18, 1 fail, 15.4 s | 18, 1 fail, 9.6 s |
| `TestClientCertTls13` | 6/6, 9.6 s | 6/6, 6.1 s |
| `TestTomcat` | 26/26, 378 s | 26/26, 205 s |

Both pre-existing failures are known and unrelated, and are the *same* test in
both arms: `TestSsl.testClientInitiatedRenegotiation` (its own open doc,
`known-issues/tomcat/testssl-client-initiated-renegotiation-20260801.md`) and
`TestClientCert.testClientCertPostZero` (doc 21's by-design residual — needs
real TLS renegotiation, which rustls refuses).

Two things observed and deliberately NOT chased here, since neither is this
doc's defect and both predate it: `TestSsl.testPost` takes ~213–312 s in both
arms (8 concurrent `SSLSocket` POSTs), and `TestTomcat.testBug51526` dominates
that class's runtime in both arms.

## Reproduction (for a future regression)

```powershell
cd C:\craton\CratonVM\apps\tomcat
$env:CRATONVM_REAL='net-sockets,aqs'; $env:CRATONVM_THREADS='-default-watchdog'; $env:CRATONVM_JIT='rootsnap-cache'
<cratonvm.exe> --java-home "<jdk>" --Xmx 2g -Dtomcat.test.basedir=output\build -Dtomcat.test.relaxTiming=true `
  -cp "$(Get-Content .suite\cp.txt)" org.junit.runner.JUnitCore org.apache.tomcat.util.net.TestSSLHostConfigCompat
```

Expect ~1 in 4 runs to wedge on an unfixed binary. The tell is not the JUnit
message but the pair of facts: the whole run takes **~325 s instead of ~26 s**,
and stderr carries exactly one `STW cross-thread JIT takeover is still waiting
for cooperative mutators` line. A run that fails in seconds is a different bug.

`CRATONVM_DBG=gc-stress=…` is **not** a usable accelerator here — it drives this
class into `OutOfMemoryError: Java heap space` at `--Xmx 2g` before the
interesting window is reached. Repeat whole-class runs are the gate.
