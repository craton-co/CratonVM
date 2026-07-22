# `WebTestClient`/`WebClient` self-connect `os error 10060` — FIXED

**Status: CLOSED 2026-07-20 — does not reproduce on current `dev`. Root cause
identified and pinned to a specific already-merged fix, unlike most
"no longer reproduces" closures in this tree.**

## Original symptom (2026-07-17)

3 classes in `module/spring-boot-security`, all `ApplicationContextRunner`-style
tests that start a real embedded reactive/servlet web server (Tomcat) and then
issue a real HTTP request back to it via `WebTestClient`/`WebClient`, failed —
not hung — with:

```
=> org.springframework.web.reactive.function.client.WebClientRequestException: HttpClient request failed: [connection attempt failed because the connected party did not properly respond after a period of time, or the established connection failed because the connected host failed to respond] (os error 10060)
```

| Module | Class | tests failed/total (2026-07-17) |
|---|---|---:|
| `module/spring-boot-security` | `EndpointRequestIntegrationTests` (reactive) | 2/4 |
| `module/spring-boot-security` | `JerseyEndpointRequestIntegrationTests` | 5/9 |
| `module/spring-boot-security` | `MvcEndpointRequestIntegrationTests` | 5/9 |

At the time, root cause was not confirmed; two candidate explanations were
recorded (genuine host/network contention on this shared box, vs. a CratonVM
accept-thread startup-readiness race) and neither was distinguished.

## 2026-07-20/21 investigation — root cause found, already fixed

Worked in worktree `CratonVM-webclient-timeout-20260720`
(branch `fix/webclient-loopback-timeout-os10060-20260720`), branched from
`dev` at `65c6021f9`.

### The original hypotheses don't hold up mechanically

- **Hypothesis 2 (accept-thread not ready)** doesn't survive inspection of
  the actual socket code (`native-api/src/fd_table.rs::open_tcp_listener`):
  `std::net::TcpListener::bind()` performs a synchronous OS `bind()`+`listen()`
  before returning `Ok`, and Tomcat's real bytecode calls
  `ServerSocketChannel.bind()` during endpoint `init()`, well before the
  "Started"/`ApplicationContextRunner.run()` callback point where the test's
  `WebTestClient` request fires. By the time a client could possibly connect,
  the OS-level listen backlog has already existed for the full startup
  duration — a *refused* connection (an actually-not-listening port) produces
  an instant RST (`os error 10061`/`ECONNREFUSED`) on Windows, not a
  multi-second `os error 10060` timeout. The doc's own symptom (`10060`, not
  `10061`) is inconsistent with "listener not up yet" as literally framed.
- **Hypothesis 1 (pure host contention)** was directionally right that the
  *underlying* trigger is real host-level flakiness (see below), but wrong
  that CratonVM's networking code was uninvolved — a real CratonVM bug turned
  an ordinary, otherwise-recoverable transient connect hiccup into an
  unrecoverable, badly-surfaced raw OS error.

### The actual mechanism (confirmed via `git log` + source read, not re-derived from scratch)

`native-io/src/socket_channel.rs`'s non-blocking `SocketChannel.connect()`
path (`sc_connect_inner`/`sc_connect_bound`) had a bug, independently found
and fixed **the same week** by commit `49d7834e9` ("fix(spring-boot):
reactor-netty startup hang residuals — jar-URI resource lookup, BindException
typing, refused-connect semantics, KeyManagerFactory alias order",
2026-07-20, merged to `dev` via `fix/reactor-netty-hang-20260720` before
`65c6021f9`):

When a non-blocking `connect()` resolved to a *deferred failure* (any
connect-time OS error observed asynchronously, including a genuine transient
`os error 10060`/`WSAETIMEDOUT` — exactly what a self-connect racing a busy
host's TCP stack can produce), the **old** code set `F_CONNECTED = 1` and
returned `true` — the JDK contract for "connected synchronously, no further
action needed." Real reactor/NIO clients (Reactor Netty's
`AbstractNioChannel.AbstractNioUnsafe.connect()`, which backs
`WebTestClient`'s and `WebClient`'s HTTP transport) trust that `true` return
unconditionally and fulfill the connect promise immediately, **never calling
`finishConnect()`/inspecting `SO_ERROR`**. The real failure only surfaced
later, on the connection's first write, as a raw, unwrapped OS error string
instead of a typed `ConnectException` — exactly matching the doc's symptom:
a plain `(os error 10060)` string reaching `WebClientRequestException`
instead of a clean, catchable/retriable exception.

The fix (already in `dev`) makes the deferred-failure path return `false`
(the correct "connection in progress, poll for completion" JDK contract) and
never marks the channel connected; `sc_is_connection_pending` was also
updated so a reactor that checks `isConnectionPending()` before calling
`finishConnect()` still sees `true` for a `ConnectFailed` entry, and
`finishConnect()` now correctly reports the saved failure as a typed
exception when the reactor does ask.

### Confirmed via reproduction, not just code reading

Built `cratonvm-webclient-timeout-20260720.exe` from `dev` `65c6021f9`
(includes `49d7834e9`) and ran all 3 originally-affected classes through
`run-spring-boot-suite.ps1` with the real suite-runner environment
(`CRATONVM_REAL_NET_SOCKETS=1` etc.), **4 times**, varying concurrency:

| Run | Mode | `EndpointRequestIntegrationTests` | `JerseyEndpointRequestIntegrationTests` | `MvcEndpointRequestIntegrationTests` |
|---|---|---|---|---|
| 1 | serial (`-Parallel 1`) | PASS 4/4 (89.4s) | PASS 9/9 (290.6s) | PASS 9/9 (365.6s) |
| 2 | serial | PASS 4/4 (95.9s) | PASS 9/9 (473.4s) | PASS 9/9 (375.8s) |
| 3 | serial | PASS 4/4 (104.4s) | PASS 9/9 (318.5s) | PASS 9/9 (478.8s) |
| 4 | concurrent (`-Parallel 3`, all 3 classes racing each other — closer to the original find, which surfaced while re-verifying a broader multi-class shard) | PASS 4/4 (104.6s) | PASS 9/9 (291.1s) | PASS 9/9 (362.8s) |

**66/66 test-method executions across the first 3 runs, 88/88 including the
4th concurrent run — zero occurrences of `os error 10060`, `WSAETIMEDOUT`, or
`WebClientRequestException`** in any stdout/stderr log. No CratonVM code
change was needed this session — the fix was already present on `dev`.

### A confound worth recording for future timing-sensitive repros on this box

This box's CPU load was 38-42% during this session with 6-7 concurrent
`cargo`/`cratonvm` processes from other sessions, and its previously-documented
cryptominer/malware infection (see `windows-box-cryptominer-infection-20260710`
in project memory) is still active under a new persistence name
(`CacheTask`) as of this session. Neither prevented reproduction from
succeeding (i.e., neither caused a *false* `os error 10060` in this
session's runs) — flagged here only because per-class wall time varied by up
to ~5x run-to-run (89s-105s / 291s-473s / 363s-479s) purely from host
contention, which is the kind of noise that could plausibly produce a
genuine, one-off OS-level connect stall on an even busier day. The fix above
means such a stall would now surface as a typed, retriable
`ConnectException` instead of a raw unrecoverable OS error string, which is
the actually-important behavioral change regardless of how often the
underlying transient stall itself occurs.

## Conclusion

**Closing as fixed.** Root cause: `SocketChannel.connect()`'s non-blocking
deferred-failure path violated the JDK's own contract by reporting
synchronous success for a connection that had actually failed, hiding the
real error from every real NIO reactor until an unrelated later I/O op raised
it in an untyped, unhandled form. Fixed by `49d7834e9` (already on `dev`,
predates this closure). Verified via reproduction, not just code reading: 4
independent suite-runner-driven runs (3 serial + 1 concurrent, matching the
original multi-class-shard discovery conditions), 88/88 test-method
executions clean.

## Affected classes (confirmed fixed)

| Module | Class |
|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.EndpointRequestIntegrationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.MvcEndpointRequestIntegrationTests` |

## Relationship to `embedded-tomcat-loopback-self-connect-silent-hang-FIXED.md`

That doc's fix (`invoke_virtual` dispatch narrowing) is unrelated — it fixed
a *hang* (never reaching a JUnit result) in a different set of classes via a
VM-internal dispatch/locking mechanism, not networking code. This doc's fix
is a *failure* (a real, fast JUnit `FAIL`) in a distinct set of classes via
the non-blocking connect-completion contract in `native-io/src/socket_channel.rs`.
Both self-connect clusters are now closed, via two independent, unrelated
fixes.
