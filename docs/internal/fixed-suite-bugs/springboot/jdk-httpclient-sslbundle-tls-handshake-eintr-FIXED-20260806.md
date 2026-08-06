# `HttpClient` TLS handshake failed with EINTR — CratonVM was interrupting itself

| | |
|---|---|
| **Status** | ✅ **FIXED** — retired from `docs/known-issues/springboot/` on 2026-08-06 |
| **Cause** | the TLS handshake is a bare `read_tls`/`write_tls` pair with no EINTR retry, on a socket carrying `SO_RCVTIMEO` — which Linux excludes from `SA_RESTART`. The signal is CratonVM's own: `jit::xt_root_scan` `SIGUSR2`s every thread for a cross-thread stop-the-world root scan |
| **Fixed by** | this branch — `cratonvm_native_io::eintr` plus 36 converted socket-backed TLS I/O sites |
| **Severity** | medium — a random mid-request `IOException` under load, on any HTTPS client or server path |
| **HotSpot** | clean — 32/32, as reported by the original page from `hotspot-baseline-latest.tsv`. Not re-measured here: that file has since been overwritten by a 6-row partial run, and a HotSpot arm cannot answer the question anyway, since the switch below is a CratonVM knob |
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
| `http_url_connection.rs` | 3 + the post-handshake request flush and the pooled-connection probe read |
| `net_phase_e.rs` | 2 + the two TLS `flush`es, and the `native_tls` socket, wrapped whole |
| `http_client.rs` | 2 + the HTTP/2 and HTTP/1.1 flushes |
| `async_socket.rs` | the two worker-thread blocking reads and the blocking accept |
| `socket_channel.rs` | `accept_close_aware` — its Unix-domain sibling had carried the arm since it was written; the TCP one never did |

Three things deliberately **not** converted:

* Every `write_all` / `read_exact` / `read_to_end`. `std`'s defaults already
  reissue on `Interrupted` **and** advance past the bytes they placed;
  `retry_eintr` can only restart the whole call, so wrapping a partially
  completed write would resend it from offset 0. The first cut of this branch
  wrapped four of them — inert (nothing that reaches them carries both a
  non-`Interrupted` kind and errno 4) but stating a contract this module cannot
  honour, so they came back out.

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

One binary, all arms, **positive control first**. Windows box,
`cratonvm-tlseintr-20260806.exe` (release, `b7706a982`), `-jit`,
`JdkClientHttpRequestFactoryBuilderTests` via
`apps/spring-boot-suite-runner/run-single-class.ps1`:

| arm | `INJECT` | `NO_RETRY` | result | seconds |
|---|---|---|---|---:|
| **positive control** | 2 | 1 | **`tests=32 failed=4`** — 8 log lines carrying the reported message | 78 |
| fix on | 2 | — | `tests=32 failed=0 containersFailed=0` | 79 |
| flag-only control | — | 1 | `tests=32 failed=0 containersFailed=0` | 81 |
| baseline | — | — | `tests=32 failed=0 containersFailed=0` | 81 |

The defect arm reproduces the 2026-08-05 report **verbatim**, on the reported
method, both parameterisations:

```
JUnit Jupiter:JdkClientHttpRequestFactoryBuilderTests:connectWithSslBundle(String):[2] httpMethod = "POST"
  => java.io.IOException: HttpClient request failed: TLS handshake read: Interrupted system call (os error 4)
```

The flag-only arm is what separates the injection from the switch: with
`NO_RETRY` set and nothing injected the class is still 32/32, so the four
failures above come from the interrupted syscall and not from the knob.

The first attempt used `INJECT=20` and was **green in the defect arm** — a
false negative. One in twenty operations is not dense enough to land on the two
`read_tls` calls of a handshake that is over in a few round trips, and a green
positive control proves nothing. `INJECT=2` lands on them every time. Anyone
re-running this: check that the defect arm goes red *before* reading anything
into the fix arm.

### Nothing else in the module moved

Same binary, no flags, then again at `INJECT=2` with the retry on — the second
sweep is the one that shows the retry holds across the Reactor Netty, Apache
HttpComponents and Jetty client stacks too, not just the JDK one:

| class | tests | baseline | `INJECT=2`, fix on |
|---|---:|---|---|
| `ReactorClientHttpRequestFactoryBuilderTests` | 33 | 0 failed | 0 failed |
| `HttpComponentsClientHttpRequestFactoryBuilderTests` | 32 | 0 failed | 0 failed |
| `JettyClientHttpRequestFactoryBuilderTests` | 32 | 0 failed | 0 failed |
| `SimpleClientHttpRequestFactoryBuilderTests` | 19 | 0 failed | 0 failed |
| `ReflectiveComponentsClientHttpRequestFactoryBuilderTests` | 21 | 0 failed | — |

Note that `connectWithSslBundle` stands an embedded Tomcat up on an `SslBundle`
connector **in the same VM**, so all four of these runs drive CratonVM's TLS
*server* path (`t27_tls`) as well as its client path — the 29 converted sites in
that file are exercised, not merely compiled.

Rust side: `cargo test -p cratonvm-types -p cratonvm-native-io` — 398 + 493
pass, including the six new `eintr` unit tests; `cargo test -p
cratonvm-native-builtins --test eintr_ratchet` — 3 pass. (`types`' two
`doc_citation_paths` guards are red on `dev` too, at 10 and 5 violations; this
branch leaves both counts unchanged.)

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
