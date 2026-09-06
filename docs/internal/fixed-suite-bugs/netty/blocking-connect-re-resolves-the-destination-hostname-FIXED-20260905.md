# Every blocking `connect` re-resolves the destination hostname, and Windows stalls after a few dozen

**Retirement re-verification, 2026-09-06.** Retired from
`docs/known-issues/netty/blocking-connect-accept-stalls-near-128-connections-20260905.md`.
The fix was re-measured on `dev` at `6430e495f` with a freshly built Windows
binary, running the page's own falsifier in BOTH directions from ONE binary and
one probe build — so the repair is attributable to the switch and not to
anything else that moved in the eight days since:

| arm | destination | result |
|---|---|---|
| fix on (default) | `InetAddress.getLoopbackAddress()` → `localhost` | **`CLIENT_OK connected=150`, 1 s** |
| fix on (default) | `InetAddress.getByName("127.0.0.1")` → literal | `CLIENT_OK connected=150`, 0 s |
| `CRATONVM_SC_PRERESOLVED=0` | `localhost` | **stalls — last line `[11] connect`, killed at the 150 s cap (rc=124)** |

The third row is the one that matters: with the pre-resolved dial switched off
on the same binary the stall comes straight back, at a fresh index (`#11` here
against `#43` on 2026-09-05), which is also a second confirmation of the "no
fixed threshold" finding below. Each arm got its own HotSpot server process, so
no arm inherited another's socket state.

Command, verbatim:

```bash
javac -d probeout probes/WinConnectStallProbe.java
java  -cp probeout WinConnectStallProbe server 400          # prints PORT=<p>
                    ./target/release/cratonvm.exe -cp probeout WinConnectStallProbe client <p> 150
                    ./target/release/cratonvm.exe -cp probeout WinConnectStallProbe client <p> 150 literal
CRATONVM_SC_PRERESOLVED=0 ./target/release/cratonvm.exe -cp probeout WinConnectStallProbe client <p> 150
```

---


**Status: FIXED 2026-09-05.** Found while building F3's
acceptance curve for `socket-transfer-per-call-costs-20260904.md`.
**Not caused by that work** — the exoneration arm is below and is one command.

The page's first revision said "connect/accept stalls near 128 connections".
Both halves of that title were wrong: it is **not** accept, and there is **no
fixed threshold**. Corrected below, with the arm that refuted each.

## What it is

`probes/WinConnectStallProbe.java` opens N loopback connections in a plain
sequential loop and prints, flushed, before every call. On Windows, CratonVM's
**client** side stops returning from `SocketChannel.open(addr)`. No exception,
no error, no progress.

## What has been established, and by which arm

| claim | arm | result |
|---|---|---|
| It is the CLIENT, not the server | HotSpot server + CratonVM client | **stalls at connect #43** |
| The server/accept side is fine | CratonVM server + HotSpot client | **200 connections OK** |
| Not the machine's socket state | HotSpot client, same moment, same probe | **200 OK** |
| Windows only | Azure Linux, same commit, idle=256 | **passes, checksum matches** |
| Not the selector | `noreg` mode, no `register()` at all | stalls identically |
| Not TIME_WAIT / ports | `Get-NetTCPConnection` | TIME_WAIT 8 vs 204 → **same index**; 16 384 ports free |
| Not leaked processes | `Get-Process cratonvm` between runs | none |
| Not GC / heap pressure | `-Xmx2g` vs default | **36 vs 36** — identical |
| Blocked, not slow | `Get-Process` CPU across 8 s | flat at 13.3 s |
| Blocked in a NATIVE call | `--stack-dump-on-timeout 45` | watchdog: `deposit=STALE … RUNNING right now … JIT-compiled code or a long native call`; `[WAIT-CENSUS] none`, so not `Object.wait()`, not a monitor |

At the stall the two processes are waiting on each other: the client printed
`[22] connect` and the HotSpot server printed `[22] accept`. **The connection
never reaches the listener.**

## There is no fixed threshold, and that is a finding

Observed stall indices across one session, same binary, same probe:

```
73, 73, 48, 48, 48, 44, 44, 43, 43, 36, 36, 22, 58
```

Deterministic within a batch, drifting between batches, and NOT explained by
TIME_WAIT (8 vs 204 gave the same index), heap, or leaked processes. **So this
is timing- or state-dependent, not a resource ceiling** — which means the
original "≈128 connections" framing was an artefact of the first probe printing
only every 32 connections. Do not go looking for a constant to raise.

## The cause

`decode_socket_address` reads the destination with
**`InetSocketAddress.getHostString()`**, which returns the HOSTNAME whenever the
address carries one. The already-resolved `InetAddress` the caller supplied is
discarded, and `sc_connect_inner` hands `policy_connect` a *name*:

```
[CONNECT-DBG] dial-enter target=localhost:52708      <- not 127.0.0.1
```

`policy_connect` then calls `to_socket_addrs()` on that name, so **every single
`connect` performs a fresh `getaddrinfo`.** HotSpot never does: an
`InetSocketAddress` built from a resolved `InetAddress` already holds the
address, and the JDK dials it directly.

On Windows the resolver stops returning after a few dozen rapid lookups, and
because the block is inside `to_socket_addrs` rather than the dial, the 30 s
`connect_timeout` never applies — which is exactly the contradiction this page
was stuck on.

### The falsifier, and it fired

Same binary, same server process, same minute. The ONLY difference is whether
the destination carries a hostname:

| destination | `getHostString()` yields | result |
|---|---|---|
| `InetAddress.getLoopbackAddress()` | `localhost` | **stalls at connect #43** |
| `InetAddress.getByName("127.0.0.1")` | `127.0.0.1` | **150/150 connect, RC=0** |

```bash
./target/release/cratonvm.exe -cp /tmp/p WinConnectStallProbe client <p> 150
./target/release/cratonvm.exe -cp /tmp/p WinConnectStallProbe client <p> 150 literal
```

This also explains every earlier observation: Windows-only (Linux resolves
`localhost` from the hosts file and does not wedge), no fixed threshold (it is
resolver state, not a resource ceiling), unaffected by heap or TIME_WAIT, a long
NATIVE call with no exception, and HotSpot unaffected on the same machine.

## It is a throughput defect too, not only a stall

Independently of the hang: **a DNS lookup on every outbound connect that
HotSpot does not perform.** That is per-connection overhead on exactly the path
this tree is trying to close a gap on, and it would not show up as a stall
anywhere the resolver happens to be fast — it would just be slower than HotSpot
for no visible reason.

## The fix, and its verification

`decode_resolved_literal` reads back the address the `InetSocketAddress`
already holds and `policy_connect_with` / `resolve_and_vet` dial it instead of
re-resolving the name. **Both** connect paths are fixed — the non-blocking one
had the same defect through `resolve_and_vet`, and that is the one netty uses,
so fixing only the blocking path would have landed at the wrong level.

The name is still what `check_outbound` sees; only the resolution is skipped.
The trace shows both halves at once:

```
[CONNECT-DBG] dial-enter target=localhost:64382 preresolved=127.0.0.1:64382
```

Verified with the falsifier running in BOTH directions, same binary, same
server process:

| arm | result |
|---|---|
| fix on, hostname (`localhost`) | **`CLIENT_OK connected=150`** |
| fix on, literal (`127.0.0.1`) — the control that already passed | `CLIENT_OK connected=150` |
| `CRATONVM_SC_PRERESOLVED=0`, hostname | **still stalls at connect #43** |

The third row is the one that matters: with the fix disabled the stall returns,
so the repair is attributable to this change and not to something else that
moved.

Regression suite, twelve net vectors, all cross-VM diffed against HotSpot:
`RJdkNet RJdkNio RSocketFastIo RChannelInterrupt RSocketChannelInterrupt
RJdkAsyncChannel RNioNoFollow RDirectBufferElem RFileChannelFastIo
RNetIfaceScope RSslLiveSession RSslNullSession` — **12 passed, 0 failed.**
`RJdkNet` was run specifically because this changes which address every
outbound connection dials: the old path tried each resolved address in turn,
and the pre-resolved path dials the one the caller chose, which is what the JDK
specifies for a resolved `InetSocketAddress` but is still a behaviour change.

## Fix direction as originally recorded, and the trap in it

Dial the address the caller already resolved instead of re-resolving its name.
`InetSocketAddress.getAddress()` returns the `InetAddress`;
`getHostAddress()` gives the literal. Fall back to `getHostString()` only when
`isUnresolved()`.

**The trap:** `policy_connect` currently vets the hostname AND every resolved
IP, and a name-based outbound policy (the worked example in its own comments is
`metadata.google.internal`) would stop seeing the name if the literal were
simply substituted at the call site. So the fix is not a one-line swap of what
`decode_socket_address` returns — it needs to keep passing the NAME to
`check_outbound` while dialling the RESOLVED address, which means threading
both through `policy_connect` rather than just changing the string.

Done that way it is also a small SSRF improvement: today's re-resolution is a
genuine TOCTOU between the address the caller vetted and the one we dial.

## Superseded lead (kept, because it was wrong in an instructive way)

`sc_connect_inner`'s blocking path routes through
`outbound_policy::policy_connect`, which dials with
`TcpStream::connect_timeout(&addr, connect_timeout())`. That timeout is
`DEFAULT_CONNECT_TIMEOUT_MS = 30_000` and the accessor is written to be
**always finite** — its own doc says *"even if an embedder calls
`set_connect_timeout(Duration::ZERO)` we fall back to the 30 s default so the
blocking thread can't hang indefinitely."*

**A run stalled for 240 s with no exception.** Both facts cannot describe the
same code, so one of them is false about the live path:

* the block is **not inside** `connect_timeout` — it is somewhere else in the
  same native call. `tcp_register`/`tcp_blocking_state` take
  `tcp_registry().write()` right after the dial, and a writer starved by a
  reader would present exactly like this: a long native call, no exception, no
  CPU. **This is the first thing to test.**
* or this path is not `policy_connect` at all. `sc_open_connected` /
  `sc_blocking_connect` / `sc_connect` are the three entry points; confirm
  which one `SocketChannel.open(SocketAddress)` reaches before assuming.

Instrumenting either is cheap: a print on both sides of the dial separates
"never dialled" from "dialled and never returned", and that single bit chooses
between the two bullets.

## Two corrections to this page's own earlier revisions

* **"`CRATONVM_DBG_NET=1` produced no output, so this path has no tracing."**
  False. The path had traces on both sides of the dial all along (`ipc_dbg`),
  gated on `CRATONVM_SUREFIRE_IPC_DBG`. The silence meant *wrong flag*, not
  *no instrument*, and reading it the other way cost most of the localisation.
  The `connect_dbg` checkpoints added since answer to BOTH flags.
* **"stalls near 128 connections."** There is no threshold; see the drift table
  above. The number was an artefact of a probe printing every 32 connections.

## Why it matters

This is the shape an HTTP keep-alive client has, and a client that stops
connecting does not raise — **it hangs**. At the harness level that is
indistinguishable from the throughput walls this tree has already had to
reclassify twice (`compression-cluster-testhugedecompress-180s-…`,
`quarkustestprofileawareclassorderer-not-a-hang-throughput-gap-…`). Any
Windows HTTP cluster whose fixture opens a few dozen client connections can
present as a timeout with no diagnostic.

## Repro

```bash
javac -d /tmp/p probes/WinConnectStallProbe.java
# server in one process, client in the other — the split is what names the half
java -cp /tmp/p WinConnectStallProbe server 200      # prints PORT=<p>
./target/release/cratonvm.exe -cp /tmp/p WinConnectStallProbe client <p> 200
# last stderr line is `[N] connect`; N drifts run to run
```

The exoneration arm, if you suspect the socket fast-I/O work:

```bash
CRATONVM_SC_SCRATCH=0 CRATONVM_SC_BB_SLOTS=0 \
  CRATONVM_SEL_READY_CACHE=0 CRATONVM_SEL_FAST_KEYS=0 \
  ./target/release/cratonvm.exe -cp /tmp/p WinConnectStallProbe client <p> 200
```

Those four revert F1/F3/F4/F5/F6 at runtime. It stalls identically.
