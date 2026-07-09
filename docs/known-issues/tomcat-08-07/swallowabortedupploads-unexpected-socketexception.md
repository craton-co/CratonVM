# TestSwallowAbortedUploads — client sees `SocketException` when none expected

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS.

## Summary

`org.apache.catalina.core.TestSwallowAbortedUploads.testAbortedPOSTOKSwallow`
fails:
```
java.lang.AssertionError: Unlimited upload with swallow enabled generates client exception
  expected null, but was:<java.net.SocketException: SocketException: Connection aborted: write0:
  Программа на вашем хост-компьютере разорвала установленное подключение. (os error 10053)>
```
(Russian OS text = "The program on your host computer aborted an established
connection" — a standard Windows WSAECONNABORTED message, localized.)

The test's premise: when the server aborts reading an oversized/invalid
upload but has "swallow input" enabled, the *client* should NOT see a
connection-reset exception while writing its POST body — the server is
expected to keep the connection alive and drain (swallow) the unwanted
upload bytes rather than abruptly closing the socket. On CratonVM, the
client's write does hit a hard `SocketException` (`os error 10053`,
`WSAECONNABORTED`), meaning the server side closed/reset the connection
instead of swallowing and draining it.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName swallowup `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.core.TestSwallowAbortedUploads
```

## Recommendation

Investigate Tomcat's "swallow input" mechanism
(`org.apache.catalina.connector.Request`'s input-swallowing path /
`Http11Processor`'s handling of an aborted request body) under CratonVM —
check whether CratonVM's socket/connector layer is closing the connection
too eagerly (e.g. on an unhandled exception during body read) instead of
continuing to read-and-discard bytes per Tomcat's intended swallow
semantics. Likely a real-socket / NIO-connector behavioral gap rather than a
pure VM correctness bug — worth checking against other socket-abort-related
findings from this investigation (`TestAccessLogValve`/`TestRewriteValve`'s
`-1` symptom, if related).

## 2026-07-09 worker isolation

Candidate root cause confirmed at the native socket contract level:
`native-io/src/socket_channel.rs::sc_close` unconditionally called
`TcpStream::shutdown(Shutdown::Both)` before dropping a Tomcat NIO
`SocketChannel`. On Windows, closing the read side while the peer is still
uploading request-body bytes is abortive/RST-prone and matches the client-side
`WSAECONNABORTED` (`os error 10053`) observed here.

Candidate fix in branch `codex/fix-tomcat0807-http-close-20260709-001`:
`sc_close` now sends only the write-side FIN and starts a bounded background
drain on a cloned stream (`lingering_channel_close`) so a client can finish
writing already-in-flight upload bytes without receiving a reset. This preserves
the earlier selector-wakeup requirement (the peer still sees EOF) without using
`Shutdown::Both`.

Focused proof run:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\target-tomcat0807-http-private-20260709-001'
cargo test -p cratonvm-native-io socket_channel::tests -- --nocapture
# 7 passed, including tomcat0807_http_lingering_channel_close_drains_peer_upload
```

Connector smoke run:

```powershell
# Unique binary:
# C:\craton\target-tomcat0807-http-private-20260709-001\debug\cratonvm-tomcat0807-http-20260709-001.exe
# Unique probe class:
# C:\craton\smoke-cache-tomcat0807-http-20260709-001\fixture\Tomcat0807HttpSmoke.java
# Result:
# TOMCAT0807_HTTP_BODY=TOMCAT0807_HTTP_SMOKE_OK
```

The full `apps/tomcat-suite-runner` checkout is not present in this worktree,
so `org.apache.catalina.core.TestSwallowAbortedUploads` itself still needs an
isolated rerun before this note can be archived.
