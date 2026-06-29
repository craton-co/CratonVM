# Bug TC0622 — UDP `DatagramChannel.socket()` adaptor missing DatagramSocket setters (`setSendBufferSize(I)V` linkage error)

> **Root cause (one line):** CratonVM's synthetic `DatagramChannel.socket()`
> returns **the channel itself** (the channel acts as its own `DatagramSocket`
> adaptor), but the channel class does **not** register the `java/net/DatagramSocket`
> tuning/bind methods that `NioReceiver.configureDatagramChannel()` calls on the
> result — `setSendBufferSize`/`setReceiveBufferSize`/`setReuseAddress`/`setSoTimeout`/
> `setTrafficClass` and the void `bind(SocketAddress)`. The first of these,
> `setSendBufferSize(I)V`, isn't declared on the real abstract `DatagramChannel`
> either, so it falls through to native lookup, finds nothing, and raises
> `NoSuchMethodError`.

**Severity:** Low/Medium (only affects Tomcat Tribes/cluster UDP receivers and any
code that drives `DatagramChannel.socket().<DatagramSocket setter>()`; here it kills
NioReceiver UDP startup).
**Status on CratonVM:** NOSUMMARY (3 classes) → **FIXED** (this change).
**HotSpot:** PASS.
**Run date:** 2026-06-23
**Fix branch / worktree:** `fix/tc0622-datagramchannel-sockopts` (`C:/craton/CratonVM-dchan`).

**Affected classes (all 3 in the "Newly surfaced" UDP cluster):**
- `org.apache.catalina.tribes.group.TestGroupChannelStartStop`
- `org.apache.catalina.tribes.test.channel.TestMulticastPackages`
- `org.apache.catalina.tribes.test.channel.TestUdpPackages`

## Symptom

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/nio/channels/DatagramChannel.setSendBufferSize(I)V"
  caller="org/apache/catalina/tribes/transport/nio/NioReceiver.configureDatagramChannel()V @pc=23"
[cratonvm] main-vm run() returned Err: Error in thread "main"
  linkage error: no such method: java/nio/channels/DatagramChannel.setSendBufferSize(I)V
```

## Call site

`NioReceiver.configureDatagramChannel()` (Tomcat Tribes NIO receiver):

```java
datagramChannel.configureBlocking(false);
datagramChannel.socket().setSendBufferSize(getUdpTxBufSize());   // <-- first failure
datagramChannel.socket().setReceiveBufferSize(getUdpRxBufSize());
datagramChannel.socket().setReuseAddress(getSoReuseAddress());
datagramChannel.socket().setSoTimeout(getTimeout());
datagramChannel.socket().setTrafficClass(getSoTrafficClass());
```

then `ReceiverBase.bindUdp(datagramChannel.socket(), port, retries)` calls
`socket.bind(addr)` — i.e. the **void** `DatagramSocket.bind(SocketAddress)`.

The compiled bytecode invokes these as `java/net/DatagramSocket.<m>`. But CratonVM's
`DatagramChannel.socket()` returns the channel object (runtime class
`java/nio/channels/DatagramChannel`), confirmed by a minimal probe:

```
channel class = java.nio.channels.DatagramChannel
socket  class = java.nio.channels.DatagramChannel   <-- socket() returns `this`
```

So virtual dispatch resolves the `DatagramSocket` methods against the
`DatagramChannel` class, which registers none of them.

## Why this is the channel's responsibility

The channel-as-its-own-socket design is intentional in CratonVM: `socket()`
returning `this` keeps `bind`/buffer settings on the single underlying UDP socket
(field 4 = `sock_id` into `SocketRegistry::dgrams`). A real `DatagramSocket`
returned here would be disconnected from the channel's socket, so `bindUdp`'s
`socket.bind(addr)` would bind the wrong socket and the receiver would never see
packets. Therefore the fix registers the `DatagramSocket` surface **on the
channel**.

## Fix

`native-builtins/src/phases_late.rs`, `register_datagram_channel`: register the
`DatagramSocket` methods that flow through `socket()` directly on
`java/nio/channels/DatagramChannel`:

- `setSendBufferSize(I)V`, `setReceiveBufferSize(I)V`, `setReuseAddress(Z)V` —
  best-effort, applied to the underlying `UdpSocket` via `socket2::SockRef` when
  the channel is already bound (no-op otherwise; in `configureDatagramChannel`
  they run *before* `bindUdp`, so the channel is typically unbound at that point —
  acceptable, they are tuning hints, not correctness-bearing).
- `setSoTimeout(I)V`, `setTrafficClass(I)V` — accepted no-ops (no effect on a
  non-blocking channel / cosmetic ToS).
- `bind(Ljava/net/SocketAddress;)V` — the void `DatagramSocket.bind`; mirrors the
  channel's existing `bind(SocketAddress)DatagramChannel` (drop any auto-bound
  socket, then `ensure_bound(host, port)`) so the receiver's UDP socket is bound
  to the requested port and `receive()` works.

## Follow-on blockers surfaced by the fix (separate subsystems)

Removing the linkage crash let the 3 classes run, exposing two further,
**pre-existing** defects in different subsystems. Each was fixed/characterised in
turn:

1. **MulticastSocket bind — `WSAEADDRINUSE` (os error 10048).**
   `McastServiceImpl.setupSocket` does `new MulticastSocket(port)`; our native
   bound via `std::net::UdpSocket::bind` **without `SO_REUSEADDR`**, but real
   `MulticastSocket` sets it before bind (so repeated bind/close cycles and
   multiple receivers can share the membership port). **Fixed:** added
   `FdTable::open_udp_reuse` (socket2: create → `set_reuse_address(true)` → bind)
   and routed the MulticastSocket constructors through it
   (`native-io/src/net.rs`).

2. **Real-JDK MulticastSocket/DatagramSocket *delegate* architecture.**
   Since JDK 14 these classes are thin wrappers that forward every op to a
   lazily-created `delegate`; CratonVM models them natively (fd_table-backed) and
   never populates the JDK `delegate`, so the concrete inherited bytecode
   (`setOption`/`joinGroup`/`send`/`receive`/…) calls `delegate()` →
   `InternalError("Should not get here")`. **Fixed:** implemented the
   DatagramSocket-surface methods Tribes drives on the real-JDK MulticastSocket
   path (`native-io/src/net.rs`: `setOption`/`getOption`/`setSoTimeout`/
   `setTimeToLive`/`joinGroup`/`leaveGroup`/`send`/`receive`, reading/writing the
   real `DatagramPacket` via its accessors) + a `force_native_over_real_jdk_bytecode`
   entry (`vm/src/runtime/interpreter.rs`) so the natives win over the broken
   inherited bytecode. `receive` maps a read timeout to `SocketTimeoutException`
   so Tribes' receiver loop continues. Added `udp_set_send_buffer_size` /
   `udp_set_recv_buffer_size` to `FdTable`.

## Validation (`run-suite.ps1`, serial, JIT-on default)

| Class | HotSpot | CratonVM before | CratonVM after |
|-------|---------|-----------------|----------------|
| `TestGroupChannelStartStop` | PASS | NOSUMMARY | **PASS** |
| `TestMulticastPackages` | PASS | NOSUMMARY | CRASH¹ |
| `TestUdpPackages` | **FAIL** | NOSUMMARY | CRASH¹ |

¹ The multicast logic itself is **correct**: under `--nojit`,
`TestMulticastPackages` reaches `SUCCESS:2000` (2000 multicast messages received,
1.61 MiB). With JIT on, both packet tests CRASH on a **separate, pre-existing
cross-thread JIT-root-scanning gap under STW GC** — NOT the datagram code.

**Precise root cause (investigated):** Tribes' `McastServiceImpl` runs the
membership send/receive on `ScheduledThreadPoolExecutor`/AQS worker threads. A
GC triggered by allocation on another thread stops the world; the collector marks
each *parked* worker from its last-published `root_snapshot`, which it gathers via
that worker's own (thread-local) `scan_active_jit_frames`. A worker that is
**mid-JIT** when STW hits — especially via an *unregistered* JIT entry (no
`JitEntryGuard`, so `gc_quiescence::is_active()` is false and the moving young
collector is chosen) — has live oops only in registers/spill slots that the
peer-run collector cannot scan. Result: those oops are relocated (moving) or
swept (non-moving) → "Stale pointer … all-zero header" for
`Thread`/`ThreadPoolExecutor`/`FutureTask`/`RunnableScheduledFuture` → heap
`PRE-corruption` → SIGSEGV. This is the `cross_thread_jit_gap` residual
(`conservative_roots.rs::warn_cross_thread_jit_gap`), the documented
"Handoff (GC/JIT)" class — a full fix needs precise per-thread JIT oop maps
captured at STW safepoints (or registered guards on every worker JIT entry).

**Partial mitigation found:** `CRATONVM_SHADOW_STACK=1` (publishes precise
shadow-stack oops into each thread's snapshot for cross-thread STW) **removes the
heap corruption + SIGSEGV** — but residual stale `Thread` pointers remain
(recovered via the interpreter's CP-class fallback) and the test still fails, so
it is not a complete fix. Not enabled here (experimental; default-off).

`TestUdpPackages` additionally fails on HotSpot, so it cannot be made green
regardless. Independent of the GC gap, neither packet test is a parity target for
this fix.

**Net result:** the assigned linkage bug is fixed; `TestGroupChannelStartStop`
goes NOSUMMARY → PASS (HotSpot parity); multicast packet exchange is functionally
correct; the remaining two crashes are a separate JIT+GC concurrency defect
(follow-up). No regression: plain `java.net.DatagramSocket.send` was already
broken identically on clean `dev` (a `net_phase_e` `re7.send` DatagramPacket
field-read bug — pre-existing, out of scope here).

Reproduce: `run-suite.ps1 -Vm craton -ListFile .tooling/dchan-list.txt -Parallel 1`.
