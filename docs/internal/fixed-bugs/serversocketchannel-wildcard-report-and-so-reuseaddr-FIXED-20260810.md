# `ServerSocketChannel` reported the wildcard as loopback and dropped `SO_REUSEADDR` — both fixed

**Status: FIXED 2026-08-10** on `fix/nio-wildcard-bind-reuseaddr-20260810`.
Filed the same day as
`docs/known-issues/springboot/serversocketchannel-binds-loopback-for-the-wildcard-and-drops-so-reuseaddr-20260810.md`,
found on the **control** row of `probes/ServerSocketPortContentionProbe.java`
(a port nothing holds) rather than any of the contention rows it was aimed at.

## The three-way result

Same host, same minute, same probe. Port differs per run only to avoid
colliding with the previous arm's `TIME_WAIT`.

```text
                      HotSpot 25.0.3            CratonVM before        CratonVM after
channel control       OK true  /[::]:p          OK false /127.0.0.1:p  OK true  /0.0.0.0:p
```

Both defects are closed: `SO_REUSEADDR` reads back `true`, and a wildcard bind
reports the wildcard.

## Defect 1 — `SO_REUSEADDR` was a silent no-op

`setOption(SO_REUSEADDR, true)` returned the channel and changed nothing;
`getOption` then answered `false`. Two halves, neither wrong-looking alone:

* `sc_set_option` recorded every option into `tcp_option_state()` **inside**
  `if let Some(id) = read_reg_id(ctx, this)`. `SO_REUSEADDR` is a *pre-bind*
  option, and before a bind a channel has no registry id — so the one option
  whose entire purpose is to be set before binding was the one option dropped.
* `sc_get_option`'s matching `else { 0 }` then reported the absence as `false`,
  and `read_option`'s `_ => Ok(0)` did the same for a bound listener (a
  listener is not a `TcpStream`, so `read_option` never saw it at all).

Fixed by:

* recording the requested value against the channel (`F_REUSEADDR`) in the
  existing GC-stable identity-hash side table, which needs no registry id;
* applying it in `ssc_finish_bind` through new `net::listener_set_reuseaddr`
  (`native-io`'s `sockopt_sys` already had cross-platform `get_raw`/`set_raw`,
  so this needed two constants, not a new dependency);
* answering `getOption` for a bound listener from the **OS** via
  `net::listener_get_reuseaddr`, falling back to the requested value only when
  the platform shim declines — never to a fabricated `false`.

**Honest limit.** The option is applied *after* `TcpListener::bind`, because
`std` owns socket creation there. On Unix `std` already sets `SO_REUSEADDR` for
a listener, so this confirms it; on Windows it does not, so the socket now
genuinely carries the option and reports it truthfully — but the bind it would
have influenced has already happened. A genuinely pre-bind option needs raw
socket creation (`socket()`/`setsockopt`/`bind`/`listen` →
`TcpListener::from_raw_socket`). Not done here.

## Defect 2 — the wildcard was reported as loopback, and that is ALL it was

The original filing said "a server that asked to listen on every interface is
listening on one". That was a reasonable reading of `getLocalAddress()` and it
was **wrong** — `getLocalAddress()` is the API under suspicion, so it cannot
witness what the socket did. Asked three ways that bypass it
(`probes/WildcardBindReachabilityProbe.java`, pre-fix build, port 19311):

```text
getLocalAddress()     = /127.0.0.1:19311           <- the lie
netstat -ano          TCP 0.0.0.0:19311 LISTENING  <- the truth
connect 127.0.0.1     = OK
connect 192.168.1.7   = OK                         <- this host's LAN address
```

`ssc_bind` always called `TcpListener::bind(0.0.0.0:port)`. The socket was
always on every interface and always reachable off-box. Only the accessor lied.

### Why the rewrite existed, and why removing it is safe

`advertised_listener_host` mapped an unspecified address to loopback **on
purpose**, on the premise that "Windows rejects a connect to an unspecified
listener address with `WSAEADDRNOTAVAIL`" — the named casualty being
`sun.net.httpserver.ServerImpl.getAddress()` feeding
`RestClientBuilderIntegTests`, which reconnects to whatever it is handed.

Both halves of that premise were measured, and neither holds:

* **HotSpot publishes the wildcard and its callers cope.** Temurin 25 answers
  `HttpServer.getAddress() == /[0:0:0:0:0:0:0:0]:p`, `isAnyLocalAddress() ==
  true` (`probes/HttpServerWildcardAddressProbe.java`). If publishing the
  wildcard broke reconnecting callers, it would break them on HotSpot first.
* **Connecting to the IPv4 wildcard works, on CratonVM too.** The
  `connect 0.0.0.0` row is `OK` on both VMs — Windows resolves a connect to the
  unspecified address as a connect to loopback. The `WSAEADDRNOTAVAIL` the
  rewrite was built to dodge does not reproduce.

So the accessor now reports what the socket is bound to. Its unit test
asserted the *old* behaviour (`0.0.0.0` → `127.0.0.1`), which is how the
rewrite survived — a test can pin a bug as easily as a fix. It now asserts the
address actually bound.

**Re-run owed:** the Elasticsearch/`RestClientBuilderIntegTests` path that
motivated the rewrite was not re-run — the measurements above say the reason
for it is gone, but they are not that suite.

## Left open — a third divergence this did NOT fix

HotSpot binds `0.0.0.0` as a **dual-stack IPv6** socket and reports `[::]`;
`ssc_bind` creates a v4 listener and reports `0.0.0.0`. Both are "the
wildcard", both satisfy `isAnyLocalAddress()`, and both accept v4 connections —
but they are not the same string, and the consequence is visible:

```text
connect ::   HotSpot: OK      CratonVM: ConnectException ::1:p Connection refused
```

A caller that publishes its listen address and reconnects over IPv6 reaches a
dual-stack listener on HotSpot and nothing on CratonVM. Tracked here rather
than fixed; it is a bind-family change (v6 socket + `IPV6_V6ONLY=false`) with
its own blast radius across `ssc_bind`, `ss_wrapper_bind` and the UDS path.
