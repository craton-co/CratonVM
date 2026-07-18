# Fixed: `SocketInputStream.read()` now reports `SO_TIMEOUT` as `SocketTimeoutException`

**Status: FIXED — 2026-07-18**

## Original symptom

`org.springframework.boot.docker.compose.lifecycle.TcpConnectServiceReadinessCheckTests.checkWhenNoSocketOutput()` failed because its intentionally idle socket returned `-1` from `InputStream.read()` after the configured 100 ms read timeout. The Spring Boot readiness check correctly treats a caught `SocketTimeoutException` as a retryable success condition, but it treats `-1` as an immediate disconnect.

## Root cause and residual audit

The legacy synthetic `java/net/SocketInputStream` registrations in `native-builtins/src/phases_early.rs` mapped `TimedOut` to EOF through their catch-all arm. On Unix, the same `SO_RCVTIMEO` expiry may surface as `WouldBlock`; that arm returned zero even though a blocking `InputStream.read` with a non-empty buffer must not return zero.

The Spring readiness check also calls `Socket.setSoTimeout(100)` before `Socket.connect(...)`. The legacy synthetic setter only configured an already-installed TCP stream and silently dropped an unconnected socket's pending setting, so the later read waited for the server's close and again appeared as EOF.

The real-JDK socket path was audited too. Its shared `re1_socket_read_stream` helper in `native-builtins/src/net_phase_e.rs` had not been returning EOF, but it did erase both timeout forms into a generic `IOException`, losing the concrete type caught by Java callers. All three real-JDK `Socket$SocketInputStream` overloads use that helper.

The `CRATONVM_REAL_NET_SOCKETS=1` route has a distinct JDK-25 `NioSocketImpl` implementation. It uses non-blocking reads and `Net.poll` to enforce its Java-level timeout. CratonVM had left `IOUtil.configureBlocking` as a no-op, so its dispatcher blocked until peer close and returned EOF instead of the JDK's unavailable sentinel.

## Resolution

- Map both `ErrorKind::TimedOut` and `ErrorKind::WouldBlock` to the concrete `RuntimeError::SocketTimeoutException` in all three legacy `SocketInputStream.read` overloads.
- Map those same timeout forms in the shared real-JDK read helper, preserving the typed exception across `read()`, `read(byte[])`, and `read(byte[], off, len)`.
- Preserve `Socket.setSoTimeout` instead of globally replacing it with a SocketChannel-adaptor no-op, and implement the real-JDK `configureBlocking`/`Net.poll` timeout cycle.
- Retain an unconnected synthetic socket's timeout in the GC-stable socket side table, apply it when `connect` installs the stream, and expose it through `getSoTimeout`.
- Add `SocketInputStreamTimeout`, a loopback regression probe with an external idle peer. It covers every overload both before and after `connect`, in synthetic and `CRATONVM_REAL_NET_SOCKETS=1` modes, with JIT and `--nojit` execution.

This restores the Java contract: a connected peer that supplies no data before `SO_TIMEOUT` throws `SocketTimeoutException`; only an actual zero-byte peer close returns EOF (`-1`).
