# `SocketInputStream.read()` treats a `SO_TIMEOUT` read timeout as EOF (-1) instead of throwing `SocketTimeoutException`

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | Failures |
|---|---:|
| `org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheckTests` | 1/4 |

```
JUnit Jupiter:TcpConnectServiceReadinessCheckTests:checkWhenNoSocketOutput()
    MethodSource [className = 'org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheckTests', methodName = 'checkWhenNoSocketOutput', methodParameterTypes = '']
    => org.springframework.boot.docker.compose.lifecycle.ServiceNotReadyException: Immediate disconnect while connecting to port 61091
       org.springframework.boot.docker.compose.lifecycle.ServiceNotReadyException.<init>(ServiceNotReadyException.java:39)
       org.springframework.boot.docker.compose.lifecycle.ServiceNotReadyException.<init>(ServiceNotReadyException.java:35)
       org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheck.check(TcpConnectServiceReadinessCheck.java:71)
       org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheck.check(TcpConnectServiceReadinessCheck.java:58)
       org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheck.check(TcpConnectServiceReadinessCheck.java:48)
       org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheckTests.check(TcpConnectServiceReadinessCheckTests.java:105)
       org.assertj.core.api.ThrowingConsumer.accept(ThrowingConsumer.java:34)
       org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheckTests.withServer(TcpConnectServiceReadinessCheckTests.java:100)
       org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheckTests.checkWhenNoSocketOutput(TcpConnectServiceReadinessCheckTests.java:68)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-docker-compose.org.springframework.boot.docker.compose.lifecycle.TcpConnectSe-48e4a6b92a93.err.log`
(`.out.log` sibling for the full JUnit summary)

## Root cause (confirmed against source)

`checkWhenNoSocketOutput()` (`TcpConnectServiceReadinessCheckTests.java:65-69`)
sets up a server thread that sleeps 10 seconds before touching the socket at
all, against a readiness check configured with `readTimeout = 100ms`
(`setup()`, line 53). The intent (per the test's own comment) is: the
client-side blocking read must time out — `TcpConnectServiceReadinessCheck.check`
(`TcpConnectServiceReadinessCheck.java:64-71`) calls
`socket.setSoTimeout(readTimeout)` then `socket.getInputStream().read()`
inside a `try { ... } catch (SocketTimeoutException ex) { /* Ignore */ }`.
A real timeout is the *success* path; only `read() == -1` (a genuine
immediate EOF/disconnect) throws `ServiceNotReadyException`.

CratonVM's native for `java.net.SocketInputStream.read()` (and its
`read([BII)I`/`read([B)I` overloads) —
`native-builtins/src/phases_early.rs:15299-15390` — does correctly set the
OS-level read timeout (`plain_socket.rs`'s `SO_TIMEOUT` handler,
lines 658-670, calls `socket2::Socket::set_read_timeout`, which programs
`SO_RCVTIMEO`), but its blocking-read error mapping only handles two
`ErrorKind`s explicitly:

```rust
match read_retry_eintr(&mut stream_ref, &mut buf) {
    Ok(0) => Ok(Some(Value::Int(-1))),
    Ok(_) => Ok(Some(Value::Int(buf[0] as i32))),
    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Some(Value::Int(0))),
    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(Some(Value::Int(0))),
    Err(_) => Ok(Some(Value::Int(-1))),   // <-- SO_RCVTIMEO expiry lands here
}
```

When the OS-level receive timeout set via `SO_RCVTIMEO` actually fires, the
underlying blocking read returns an I/O error whose `ErrorKind` is
`TimedOut` (on Windows this is the mapped form of `WSAETIMEDOUT`) — not
`WouldBlock`. There is no arm for `ErrorKind::TimedOut` here, so it falls
into the catch-all `Err(_) => Ok(Some(Value::Int(-1)))`, which returns
Java `-1` (i.e. `read()` reports EOF) instead of throwing
`java.net.SocketTimeoutException`. All three `read` overloads registered in
this block (`()I`, `([BII)I`, `([B)I`) share the identical pattern and the
identical gap.

This is exactly what the test observes: the 100ms read times out, this code
silently reports EOF instead of raising `SocketTimeoutException`, the
`catch (SocketTimeoutException ex)` in `TcpConnectServiceReadinessCheck.check`
never fires, and the `if (read() == -1) throw new ServiceNotReadyException(...)`
branch fires instead — producing exactly the observed
"Immediate disconnect while connecting to port …" failure for a connection
that never actually disconnected, only timed out as designed.

Other native socket/network paths in this codebase already do this mapping
correctly (`ErrorKind::TimedOut => RuntimeError::SocketTimeoutException` in
`native-builtins/src/net_phase_e.rs:3133`, `native-io/src/net.rs:682`, and
`native-io/src/socket_channel.rs:247`) — this specific
`java/net/SocketInputStream.read*` registration in
`native-builtins/src/phases_early.rs` is the one call site that never
received the same treatment.

## Suggested fix direction

Add an explicit `Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Err(RuntimeError::SocketTimeoutException { .. }.into())` arm
to all three `read` overloads at `native-builtins/src/phases_early.rs:15299-15390`
(and audit any other `SocketInputStream`/`SocketOutputStream` read paths in
the same file for the same gap), mirroring the mapping already used in
`net_phase_e.rs`/`native-io/src/net.rs`/`native-io/src/socket_channel.rs`.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-docker-compose` | `org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheckTests` |
