# `TestHttpServletDoHeadInvalidWrite1023ValidWrite1023` `testDoHeadHttp2[0]` — likely shared-host-contention false positive, NOT confirmed as a CratonVM bug

| | |
|---|---|
| **Status** | 🟡 UNCONFIRMED / likely NOT a CratonVM bug — does not reproduce in isolation; best evidence points to shared-host contention. Left open rather than fixed. |
| **Area** | HTTP/2 connection preface (`Http2TestBase.validateHttp2InitialResponse`) over CratonVM's `native-io` socket-channel layer |
| **Sibling finding** | The other failure in the same log (`testDoHeadHttp2[25]`, `Thread.setPriority()` NPE) was a **confirmed, deterministic, real** bug — fixed separately, see `../../../../native-builtins/src/lib.rs`'s `populate_real_thread_holder` (merged to `dev` at `3e7f4a30`, commit `34faddd9`). This doc covers only the other, unrelated failure. |

## Original symptom

One fresh `real-jit` run of the 288-parameter class (`apps/tomcat/.suite/results/doheadsib2/real-jit/...log`, `Tests run: 288, Failures: 2`) showed:

```
1) testDoHeadHttp2[0: 0 false false 16 false 1,023 NONE 1,023 false](jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1023ValidWrite1023)
java.net.SocketException: SocketException: Connection aborted: read0: ... (os error 10053)
	at sun.nio.ch.NioSocketImpl.implRead(NioSocketImpl.java:319)
	at org.apache.coyote.http2.Http2TestBase$TestInput.fill(Http2TestBase.java:1094)
	at org.apache.coyote.http2.Http2Parser.readFrame(Http2Parser.java:77)
	at org.apache.coyote.http2.Http2TestBase.validateHttp2InitialResponse(Http2TestBase.java:167)
	at org.apache.coyote.http2.Http2TestBase.http2Connect(Http2TestBase.java:149)
	at jakarta.servlet.http.HttpServletDoHeadBaseTest.testDoHeadHttp2(HttpServletDoHeadBaseTest.java:131)
```

`os error 10053` = Windows `WSAECONNABORTED` ("An established connection was aborted by the software in your host machine") — the connection dies during the very first HTTP/2 connection-preface/SETTINGS-frame read, before any DoHead-specific servlet logic (buffer sizes, write counts) has been exercised at all. Real HotSpot passes all 288 tests in this class cleanly (`apps/tomcat/.suite/results/overnight0629c/hotspot-jit/...log`: `OK (288 tests)`).

## What was ruled out

- **Accept-queue / listener-not-ready race**: `ssc_bind` (`../../../../native-io/src/socket_channel.rs`) does a synchronous, blocking `TcpListener::bind()` before returning — the OS backlog is live long before the Acceptor/Poller threads reach `accept()`/`poll()`. No async "bound but not yet listening" gap found.
- **`Http2TestBase` client-side timing**: connects immediately after `tomcat.start()` returns with no retry/backoff, identical to HotSpot's own behavior. Not a client-side artifact.
- **A confirmed-but-likely-unrelated latent gap**: `SO_REUSEADDR` is a genuine no-op in `native-io/src/socket_channel.rs::apply_option` (`"SO_REUSEADDR" => Ok(())`) — `ssc_bind` uses plain `std::net::TcpListener::bind()`, which has no pre-bind hook to actually set that option (would need the `socket2` crate, already present transitively but not a direct dependency of `native-io`). This is real and worth fixing on its own merits, but every test in this suite binds a **fresh ephemeral port** (`connector.setPort(0)`), so same-port reuse pressure isn't in play here — this almost certainly isn't the direct cause of a same-run connection abort. Not fixed as part of this investigation; flagged separately.
- **A deterministic construction/logic bug**: none found. Unlike the sibling `testDoHeadHttp2[25]` NPE (100% deterministic, reproduced immediately in a bare-minimum standalone repro), this failure does **not** reproduce:
  - 3/3 isolated single-parameter reruns (`testDoHeadHttp2[0]` alone, via a custom JUnit `Filter` + `Request.aClass`, bypassing the other 287 parameterizations) — all passed cleanly, `Failures: 0`.
  - A full-class rerun (`apps/tomcat/.suite/results/dohead1023recheck/real-jit/...`) did **not** reproduce the `SocketException` either — but it also failed to *complete* within a 600s timeout (vs. the clean 329s baseline), stalling somewhere around test index 109/288. At the moment this was observed, the host was at **100% CPU load** with multiple concurrent `cargo`/`rustc` build processes and an unrelated `cratonvm-*` test process all running simultaneously (other automated sessions sharing this box — this repo routinely runs many concurrent worktree builds/tests, see `reference_azure_build_host`/`reference_worktree_build_recipe`).

## Conclusion

This looks like the same class of false positive already documented in
`../hibernate/hib-delayedcdisupporttest-weld-bootstrap-hang-NOT-A-BUG.md`: a
timing-sensitive test (here, a TCP connection during embedded-Tomcat startup)
intermittently fails/slows down under heavy shared-host CPU contention in a
way that's indistinguishable from a genuine defect unless the load at the
*exact moment of failure* is checked. Windows' `WSAECONNABORTED` in
particular is a classic symptom of a listening process not servicing its
accept/read loop promptly enough (scheduler starvation), not necessarily an
application-level bug.

**Not confirmed as a refutation** in the same ironclad way as the Weld case
(that investigation caught a *second* test failing identically while directly
observing high load at that moment; here the original failing run predates
this investigation, so there's no direct load-average correlation with the
*original* failure — only with a *reproduction attempt* that also slowed down
under contemporaneous heavy load). Left as an open, low-confidence item
rather than either "fixed" or "hard-refuted."

## If revisited

- Re-run the full class (or just this parameterization, isolated, several
  times) on an otherwise-idle host and check `uptime`/`tasklist` CPU load at
  the moment of any failure before assuming a CratonVM defect.
- If it reproduces cleanly on an idle host, the next lead is CratonVM's
  Acceptor/Poller thread scheduling latency under load (`NioEndpoint.java`'s
  real Tomcat Acceptor/Poller threads, backed by `native-io`'s
  `ssc_accept`/selector-registration path) — specifically whether a
  sufficiently-delayed `accept()`/first-read causes the *client* side
  (`Http2TestBase.http2Connect`) to hit its own read timeout and issue a RST,
  which would present as exactly this `os error 10053` on the server's next
  read.
- Separately, consider actually wiring `SO_REUSEADDR` through `socket2`
  in `ssc_bind` regardless of whether it's related to this bug — it's a
  correctness gap in its own right for any code that rebinds the same port
  quickly (not exercised by this port-0-per-test suite, but plausible
  elsewhere).

## Repro attempts (for anyone re-investigating)

```powershell
# Full class (authoritative, ~330s on an idle host):
apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real `
  -Category all -RunName <name> -Start 38 -Count 1 -TimeoutSec 600 -Parallel 1

# Fast isolated single-parameter rerun (seconds, not minutes) — compile a small
# JUnit driver using Request.aClass(cls).filterWith(a Filter matching method
# names starting with "testDoHeadHttp2[0:"), run via:
cratonvm.exe -cp <apps/tomcat/.suite/cp.txt classpath>;<driver classes dir> \
  -Dtomcat.test.basedir=... -Dtomcat.test.temp=... -Dtomcat.test.tomcatbuild=... \
  --add-opens java.base/java.lang=ALL-UNNAMED --add-opens java.base/java.io=ALL-UNNAMED \
  --add-opens java.base/java.util=ALL-UNNAMED --add-opens java.base/java.util.concurrent=ALL-UNNAMED \
  <YourDriverClass> jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1023ValidWrite1023 "[0:"
```
