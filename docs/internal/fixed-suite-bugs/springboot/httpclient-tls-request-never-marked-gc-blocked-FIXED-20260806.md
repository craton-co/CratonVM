# An HTTPS request through `java.net.http.HttpClient` blocked in `recv` while the GC still counted the thread as a cooperative mutator

| | |
|---|---|
| **Status** | ✅ **FIXED** — this branch |
| **Cause** | two natives that park a thread in a blocking syscall without marking it GC-blocked: `net_phase_e::http_exchange_rustls` (the whole HTTPS exchange) and `t27_tls::rustls_server_accept` (the accept poll loop, unbounded) |
| **Symptom** | `STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1 taken=0`, repeating forever. The pause never completes and the process never exits |
| **Severity** | medium — needs a collection to land inside a request. Invisible at ordinary GC rates, **100%** at `CRATONVM_DBG_GC_STRESS=1048576` |
| **Found** | 2026-08-06, closing [`jdkclienthttprequestfactory-certificaterequired-alert`](jdkclienthttprequestfactory-certificaterequired-alert-FIXED-20260806.md) — the GC-stress lever that proved that defect wedged on this one |

## The rule this breaks

A native that blocks must tell the collector, or a stop-the-world pause waits
for a safepoint the thread cannot reach until its peer answers — and in these
workloads the peer is another thread *in the same process*, which does stop at
the barrier. Mutual deadlock.

`re5_do_request` knew this. Its plain-HTTP branch brackets the exchange with
`begin_blocking_region`/`end_blocking_region`, under a long comment recording
the 2026-07-15 deadlock that put it there. Its TLS branch deliberately did not:

> Keep this native context active and do not mark the thread as GC-blocked
> while that happens.

The reasoning was sound as far as it went — the TLS handshake calls back into
Java (`JavaKeyManagerResolver::resolve` → `chooseClientAlias`) to choose the
client certificate, and a GC-blocked thread must not run bytecode. But the
conclusion drawn was "so mark nothing", which leaves connect + handshake +
write + read — up to the entire request timeout — uncooperative.

## The split that resolves it

`rustls` does its socket I/O in `read_tls` / `write_tls` / `complete_io` and its
protocol work — including every Java upcall — in `process_new_packets`, which
touches no socket. **So the region belongs around the syscall, not around the
exchange.**

This is not a new idea in this tree. `http_url_connection.rs` — the
`HttpsURLConnection` client — already does exactly this, and its comment states
the same argument, including the failed first attempt:

> the regions cover ONLY socket syscalls … They never cover
> `process_new_packets()`, which is the one place rustls can call back into
> Java … the earlier attempt that deadlocked the class-loading/vtable-install
> locks had put the region around `process_new_packets` itself — i.e. exactly
> backwards.

`java.net.http.HttpClient` was simply never given the same treatment. Same
defect, same file family, one client stack apart — the pattern the EINTR page
named: *each pass fixes the primitive it was looking at and leaves the
siblings*.

## The fix

`GcBlockingSocket<S>` (`net_phase_e.rs`) wraps the socket so every `read` /
`write` / `flush` on it is bracketed, and nothing else is. Placing it on the
socket rather than at each call site means no layer above — `rustls`,
`complete_io`, `HttpDeadlineReader` — can widen the region by accident, the
same argument that put `EintrIo` on the socket. The bracket is driven through
the native context that `set_active_native_context` already publishes for the
resolver (`t27_tls::gc_blocked_syscall`), so no signature had to change.

Also marked, because they park with no bracket at all:

| site | wait |
|---|---|
| `http_exchange_rustls` — `http_connect_with_deadline` | DNS + `connect`, up to the remaining deadline |
| `rustls_server_accept` — the non-blocking accept poll loop | **unbounded**: 20 ms sleep, forever, until a peer connects |
| `rustls_server_accept` — the inline server handshake `read_tls`/`write_tls` | the socket's 30 s timeout |

The accept loop gets **one** region for the whole wait rather than one per
20 ms tick: its body is pure Rust with no Java in it, and re-entering per tick
would deposit a root snapshot and retire the TLAB 50 times a second for a
thread that is doing nothing.

## Verification

`probes/HttpClientClientAuthProbe.java` at `CRATONVM_DBG_GC_STRESS=1048576`,
Windows, two binaries built from this branch.

| arm | binary | result |
|---|---|---|
| **pre-fix (red control)** | `cratonvm-tlsblock-prefix-20260806.exe` | **wedged** — killed at 300 s, no verdict line, `STW cross-thread JIT takeover` present |
| fix, 3 iterations | `cratonvm-tlsblock-20260806.exe` | `PROBE-OK` 3/3, **0** STW warnings |
| fix, 10 iterations | same | `PROBE-OK` 10/10, **0** STW warnings |
| fix, `--nojit` | same | `PROBE-OK`, **0** STW warnings |

### The resolver is still consulted — asserted, not assumed

A fix that quietly stopped `JavaKeyManagerResolver` from running would look
identical at this level and would silently reintroduce
[`jdkclienthttprequestfactory-certificaterequired-alert`](jdkclienthttprequestfactory-certificaterequired-alert-FIXED-20260806.md)
(the client presents no certificate; a `clientAuth=NEED` peer answers
`CertificateRequired`). So every green above was taken with
`CRATONVM_DBG_TLS_AUTH=1` and counted:

| run | iterations | `chooseClientAlias[0] -> Some` | `km_alias_material … chain_len=Some` |
|---|---:|---:|---:|
| fix, 3 iterations | 3 | **3** | **3** |
| fix, `--nojit` | 3 | **3** | — |
| fix, 10 iterations | 10 | **10** | — |

One resolver consultation per request, each producing real certificate
material. The Java callback window survives the change.

### Regression

| check | result |
|---|---|
| `JdkClientHttpRequestFactoryBuilderTests` (the class both TLS pages are about) | **3 x `tests=32 failed=0 containersFailed=0`** |
| `cargo test -p cratonvm-native-builtins --lib` | **3298 passed, 0 failed**, 6 ignored |
| throughput, no GC stress, interleaved, 20 requests/run | pre 41.92 / 41.76 / 41.66 s (median 41.76) · post 41.75 / 42.00 / 41.86 s (median 41.86) — **+0.2%, no measurable cost** |

The throughput row is the one that matters for the design: a blocked-region
enter/leave per socket syscall sounds expensive (it takes a lock, deposits a
root snapshot and retires the TLAB), and at HTTPS request granularity it is
not measurable.

## What the probe actually wedges on — and why that is worth writing down

**The wedge this probe demonstrates is not the branch the investigation
started from.** With only the client-side fix in place it still wedged at
1 MiB, and the trace said why: the last line was `attach_trust_managers_to_ctx`
on the **main** thread — which was running Java and would have cooperated — and
`chooseClientAlias` had been reached **zero** times, i.e. no client handshake
had begun at all. `pending=1` was the probe's **acceptor** thread, parked in
`rustls_server_accept`'s 20 ms poll loop, in a native, forever.

So it is the **accept** loop that this probe's completion turns on. Anyone
re-running the table above should know which lever moved which number, and not
read that green as evidence for the other half.

### Separate evidence for the client-side half

To exercise the client path without `rustls_server_accept` anywhere in it:
`JdkClientHttpRequestFactoryBuilderTests` at `CRATONVM_DBG_GC_STRESS=8388608`.
Its server is embedded Tomcat on `Http11NioProtocol`/`NioEndpoint` — the
SSLEngine path, which never calls `SSLServerSocket.accept`.

| arm | `STW cross-thread JIT takeover` warnings | completed in 1500 s |
|---|---:|---|
| pre-fix | **3** | no |
| fix | **0** | no |

The pathology is present on one side and absent on the other, on a workload the
accept fix cannot touch. **Neither arm finished**, though: 8 MiB stress makes
this class far slower than its normal ~80 s, so this is evidence about the
condition, not a completion A/B, and it is reported as such. A single-method
variant was attempted to get a completing pair and the method runner does not
take the harness plumbing this needs; it was not worth another cycle given the
client-side change is also the exact shape `http_url_connection.rs` already
ships for the same reason.

Two near-misses worth recording, both instances of "check what you are
actually measuring":

* `SSLSocketInputStream.read`'s refill was bracketed for this identical hang
  (`tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED`) — that pass
  fixed the **read** on an accepted socket and left the **accept**.
* The server stream is deliberately **not** wrapped in `GcBlockingSocket`: its
  reads are already bracketed one layer up, in `ssl_security.rs`, and wrapping
  the socket too would just nest regions on an already-fixed path.
