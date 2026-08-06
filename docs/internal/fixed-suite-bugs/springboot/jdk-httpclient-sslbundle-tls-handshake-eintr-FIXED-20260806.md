# `HttpClient` TLS handshake failed with EINTR — CratonVM was interrupting itself

| | |
|---|---|
| **Status** | ✅ **FIXED** — retired from `docs/known-issues/springboot/` on 2026-08-06 |
| **Cause** | the TLS handshake is a bare `read_tls`/`write_tls` pair with no EINTR retry, on a socket carrying `SO_RCVTIMEO` — which Linux excludes from `SA_RESTART`. The signal is CratonVM's own: `jit::xt_root_scan` `SIGUSR2`s every thread for a cross-thread stop-the-world root scan |
| **Fixed by** | this branch — `cratonvm_native_io::eintr` plus 36 converted socket-backed TLS I/O sites |
| **Severity** | medium — a random mid-request `IOException` under load, on any HTTPS client or server path |
| **HotSpot** | clean (32/32, `hotspot-baseline-latest.tsv`) |
| **Filed** | 2026-08-05, OPEN, "root cause not yet pinned — needs further investigation" |

## What it was

```
java.io.IOException: HttpClient request failed: TLS handshake read: Interrupted system call (os error 4)
	at org.springframework.http.client.JdkClientHttpRequest.executeInternal(JdkClientHttpRequest.java:118)
	at org.springframework.boot.http.client.AbstractClientHttpRequestFactoryBuilderTests.connectWithSslBundle(...:119)
```

`os error 4` is `EINTR`: a blocking `recv` that a signal interrupted **before it
transferred a single byte**. POSIX requires the caller to reissue the call — it
is not a transport error, and neither `java.net` nor `javax.net.ssl` has any
state for it, which is why the JDK's own native layer retries below the Java
surface. CratonVM's TLS handshake did not:

```rust
let count = tls.conn.read_tls(&mut tls.sock)   // <- one shot, no retry
    .map_err(|e| io::Error::new(Other, format!("TLS handshake read: {e}")))?;
```

The original page's guess — "more likely a raw `libc::read`/`recv` … bypassing
`std`'s retry" — was looking one layer too low. `std`'s `Read for TcpStream` is
a thin `recv` wrapper and does **not** retry `Interrupted`; only the *composite*
helpers (`read_to_end`, `read_exact`, `write_all`) do. There was no exotic FFI
call to find. A plain `rustls` handshake on a plain `std::net::TcpStream` is
enough.

## Why `SA_RESTART` did not save it, and where the signal came from

Both halves of the answer were already written down in this tree, in two
comments that had never been read together:

* `vm/src/jit/xt_root_scan.rs` — CratonVM `SIGUSR2`s **every thread** to take it
  over for a cross-thread JIT root scan. The handler is installed with
  `SA_SIGINFO | SA_RESTART`.
* `native-io/src/net.rs::net_poll_raw` (AUDIT 2026-08-02) — "`poll(2)` is NEVER
  auto-restarted by `SA_RESTART`, so any signal delivered to a thread parked
  here comes straight back as EINTR — and CratonVM sends one on purpose."

The handshake is not `poll`, so `SA_RESTART` looks like it should cover it. It
does not, for the second reason in Linux `signal(7)`: a socket call is **not**
restarted when a receive or send timeout (`SO_RCVTIMEO` / `SO_SNDTIMEO`) has
been set on the socket. Every HTTP and TLS client path in this tree sets one
before each read, deliberately, to keep the caller's deadline honest —
`http_exchange_rustls` does it twice per handshake iteration. So the one
mechanism that makes CratonVM's deadlines trustworthy is exactly what strips
the restart guarantee from its handshakes.

That also explains the reported shape: 1 of 32 methods, `POST` and not `GET`,
not reproducible on demand. Nothing about `POST` matters; it is whichever
handshake happens to be parked when a root scan fires.

## The same audit had already been here twice

| date | site | symptom it fixed |
|---|---|---|
| 2026-07-26 | `net::read0` / `write0` | `SocketException: read0: Interrupted system call` |
| 2026-08-02 | `net::net_poll_raw` | 14/160 `RequestMappingMessageConversionIntegrationTests` |
| **2026-08-06** | **every socket-backed `read_tls`/`write_tls`** | **this page** |

Each pass fixed the primitive it was looking at and left the siblings. The
handshake was never a `read0`, never a `poll`, and so was never in scope. This
time the retry is a shared module rather than another inline `match` arm.

## The fix

`native-io/src/eintr.rs` — `is_eintr`, `retry_eintr`, and the `EintrIo`
(borrowed) / `EintrStream` (owned) adapters. Wrapping the *socket* rather than
patching each caller means no layer above it — `rustls`, `native_tls`, or a
hand-written loop — can observe the condition, including `rustls`' internal
`complete_io`, which no per-call-site patch can reach.

Converted:

| file | sites |
|---|---|
| `t27_tls.rs` | 29 — client connect, server accept, `wrap_existing_socket`, and the server-side `SSLSocket` stream read/write |
| `http_url_connection.rs` | 3 + the post-handshake request write/flush and the pooled-connection probe read |
| `net_phase_e.rs` | 2 + `write_all`/`flush`, and the `native_tls` socket, wrapped whole |
| `http_client.rs` | 2 + the HTTP/2 flushes |
| `async_socket.rs` | the two worker-thread blocking reads and the blocking accept |
| `socket_channel.rs` | `accept_close_aware` — its Unix-domain sibling had carried the arm since it was written; the TCP one never did |

Two things deliberately **not** converted:

* The three in-memory `read_tls`/`write_tls` calls — the `SSLEngine`
  `wrap`/`unwrap` lane feeds `rustls` from a `Cursor`, which cannot be
  interrupted. `native-builtins/tests/eintr_ratchet.rs` freezes both groups so
  they stay distinguishable.
* `net_phase_e::HttpDeadlineReader::read`, which still passes EINTR **up**. Its
  caller re-arms `SO_RCVTIMEO` from the deadline on every pass, and Linux
  restarts the receive timer after each interrupted `recv` — retrying in place
  would skip the re-arm and could stretch the caller's deadline without bound.
  That loop now uses the shared `is_eintr` predicate instead of a bare
  `ErrorKind` comparison, which also catches an errno 4 that lost its kind.

## Reproduced by switching the defect, not by waiting for it

A quiet green settles nothing here, and on Windows it is worse than quiet — it
is **vacuous**. Windows has no `EINTR`, so this class passing on the Windows box
carries no information about this defect at all. It did:
`JdkClientHttpRequestFactoryBuilderTests` is row 21 of the 46-class non-passed
rerun and is not among that run's three residuals
([`RESULTS-20260806-nonpassed46-windows`](../../../../apps/spring-boot-suite-runner/RESULTS-20260806-nonpassed46-windows.md)),
which says nothing either way.

That is the same lesson the sibling page
([`reactor-netty-outbound-request-line-corruption-RESOLVED-20260806`](reactor-netty-outbound-request-line-corruption-RESOLVED-20260806.md))
was written to record — and this defect additionally needs a JIT root scan to
land inside a handshake window a few hundred microseconds wide. So the switch is
built in, and it produces the identical evidence on either host:

| knob | effect |
|---|---|
| `CRATONVM_DBG_EINTR_INJECT=<n>` | synthesise an `EINTR` on every *n*-th operation through `eintr` (floor of 2) |
| `CRATONVM_DBG_EINTR_NO_RETRY=1` | do not absorb it — i.e. behave exactly as the code did before this branch |

One binary, both arms, **positive control first**.

<!-- RESULTS -->

## Affected classes

- `module/spring-boot-http-client` —
  `org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests`
  (1/32 on 2026-08-05: `connectWithSslBundle(String="POST")`).

## See also

- [`reactor-netty-outbound-request-line-corruption-RESOLVED-20260806`](reactor-netty-outbound-request-line-corruption-RESOLVED-20260806.md)
  — the sibling failure of the *same test method* on the Reactor Netty builder.
  The original page asked whether the two shared a socket/TLS I/O primitive.
  They do not: that one was a recycled `JitInvokeInfo` address letting one call
  site serve another's dispatch, fixed by `383e7f5cf`. This one never touched
  dispatch.
- [`bounded-socket-operations-hang-FIXED-20260805`](../net/bounded-socket-operations-hang-FIXED-20260805.md)
  — the `net_poll_raw` EINTR arm is one of the two halves of that fix.
