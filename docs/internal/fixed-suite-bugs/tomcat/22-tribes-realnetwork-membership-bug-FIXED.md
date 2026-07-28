# Tribes real-network membership/coordination bug — 2 classes

**Status:** ✅ **FIXED** (2026-07-27, branch
`fix/tomcat-tribes-membership-20260727`). Three independent CratonVM↔HotSpot
fidelity gaps in the datagram/listen-socket layer, each confirmed by isolated
repro against a same-fixture HotSpot control. Both classes now PASS, including
through the exact suite-runner command this doc originally prescribed.

## Symptom (as filed)

- `org.apache.catalina.tribes.group.interceptors.TestTcpFailureDetector` —
  `testTcpFailureMemberAdd`:
  ```
  java.lang.AssertionError: Expecting member count to not be equal expected:<1> but was:<0>
  ```
- `org.apache.catalina.tribes.group.interceptors.TestNonBlockingCoordinator` —
  `testCoord1`:
  ```
  java.lang.AssertionError: Member count expected to be equal. expected:<9> but was:<4>
  ```

Both classes exercise Apache Tribes' real UDP-multicast + TCP group-membership
protocol under `CRATONVM_REAL_NET_SOCKETS=1`. The original "expected member
count higher than actual" reading was right; the guess that it was one shared
multicast/keep-alive gap was not — there were three separate causes, and the
dominant one was not in the multicast path at all.

Note on the original evidence: the runs this doc was filed from were 8-parallel
full-suite runs, so their logs also carried members from *other* concurrently
running test JVMs (all Tribes tests share multicast group `228.0.0.4:45564` and
the 4000+ receiver port range). That cross-talk added two failure lines that do
not reproduce in isolation. **Always repro these classes one-at-a-time.**

## Root causes

### 1. A datagram receive timeout was not a `SocketTimeoutException`

`MulticastSocket.receive` reported an expired `SO_TIMEOUT` as a plain
`java.io.IOException` whose *message text* began with `"SocketTimeoutException:
…"`. `McastServiceImpl.receive()` catches the concrete
`java.net.SocketTimeoutException` to treat an idle poll as normal; a bare
`IOException` escapes that catch, so `ReceiverThread` treated the **normal exit
of every idle receive** as a receive failure:

- logged `Error receiving mcast package. Sleeping 500ms`,
- slept 500 ms (leaving the membership socket listening only half the time),
- skipped the `checkExpired()` at the end of `receive()`,
- and after `recoveryCounter` (10) such "errors" handed the service to
  `RecoveryThread`, which **stops and restarts membership altogether**.

Isolated, single-class runs counted **57** such warnings in
`TestTcpFailureDetector` and **450** in `TestNonBlockingCoordinator` — versus
**0** on HotSpot in the same fixture.

Fix: `native-io/src/net.rs::udp_recv_error` classifies `ErrorKind::TimedOut`
(Windows `WSAETIMEDOUT`) and `ErrorKind::WouldBlock` (Unix `EAGAIN`) as
`RuntimeError::SocketTimeoutException`; everything else stays a plain
IOException, which is what the JDK does. The same classification was applied to
`java.net.DatagramSocket.receive`
(`native-builtins/src/net_phase_e.rs::udp_recv_ex`), which had the identical
text-prefix bug.

### 2. `bind` walked the whole address list instead of binding one address

This was the dominant cause. `ssc_bind` handed a *host string* to
`TcpListener::bind`, which tries **every** address the name resolves to and
binds the first that succeeds. `localhost` resolves to both `::1` and
`127.0.0.1`, so a second bind to a live port did not fail — it silently landed
on the other family's loopback. The JDK binds the single `InetAddress` inside
the `InetSocketAddress` and throws `BindException`.

Tribes' `ReceiverBase` auto-bind loop finds its listen port purely by catching
that `BindException` and retrying 4000 → 4001 → …, so the fallback let *every*
channel in the process claim port 4000:

```
CratonVM  TestTcpFailureDetector:  6 binds, all  localhost:4000
HotSpot   TestTcpFailureDetector:  4000, 4001
CratonVM  TestNonBlockingCoordinator: 10 channels -> only 5 distinct ports
```

Every channel then announced the same `tcp://host:4000` member identity over
multicast, each channel recognised its peers as itself, and membership never
converged — exactly the "0 members" / "4 of 9" symptoms.

Reduced repro (`BindProbe`): two `ServerSocketChannel`s binding
`localhost:45900`. HotSpot → second throws `BindException`. CratonVM before the
fix → `first: BOUND local=::1:45900`, `second: BOUND local=127.0.0.1:45900`.

Fix: `native-io/src/socket_channel.rs::single_bind_addr` resolves a listen
target to exactly one `SocketAddr` (literal used as-is; a name resolved with
IPv4 preferred, matching `InetAddress.getByName`'s default ordering and the
suite's `-Djava.net.preferIPv4Stack=true`). `ssc_bind` additionally prefers the
numeric address already resolved into the `InetSocketAddress`
(`getAddress().getHostAddress()`) over its hostname text, since a *name* is
what made the bind ambiguous. `net_bind0` in `native-io/src/net.rs` got the
same one-address rule.

### 3. A mutex held across the blocking `recv_from` serialized send behind receive

`FileEntry::UdpSocket` wrapped the socket in a `Mutex`, and `udp_recv` held it
for the entire blocking `recv_from`. Every `std::net::UdpSocket` method this
table calls takes `&self`, so the lock bought no safety — it only stalled any
concurrent `send` on the same socket for up to the receive timeout.
`McastServiceImpl` is exactly that shape: one socket, a receiver thread polling
on a 500 ms `soTimeout` and a sender thread announcing every 500 ms.

Measured with `ConvergeProbe` (time from channel2's first announcement to
channel1 registering it):

| | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| typical | 1–18 ms | 0–99 ms | 0–30 ms |
| outliers | none in 6 | **+499 ms, +1023 ms** (1–2 poll intervals) | none in 10 |

`testTcpMcastFail` asserts symmetric membership a hard 1000 ms after start with
no tolerance loop, so a one-poll-interval delay tripped it: **4/6 pass** before,
**10/10 after**. HotSpot: 6/6.

Fix: `native-api/src/fd_table.rs` stores `UdpSocket` directly rather than behind
a `Mutex` (19 call sites, all local to that file).

## Verification

All runs one-class-per-process on the Windows host, real JDK 25,
`CRATONVM_REAL_NET_SOCKETS=1`, JIT on.

| check | before | after | HotSpot control |
|---|---|---|---|
| `TestTcpFailureDetector` | FAIL | **PASS** ×10/10 | PASS ×6/6 |
| `TestNonBlockingCoordinator` | FAIL (145 s) | **PASS** ×5/5 (~24 s) | PASS (8.8 s) |
| all 17 `org.apache.catalina.tribes.*` classes | 2 FAIL | **matches HotSpot** | — |
| doc's own repro (`-Parallel 2`, suite runner) | FAIL, FAIL | **PASS, PASS** | — |
| suite runner, 900 s, incl. `TestEncryptInterceptorLargeHeap` | FAIL/FAIL/HANG | **PASS ×3** | — |

`TestEncryptInterceptorLargeHeap` fails on **both** VMs at the ad-hoc `-Xmx2g`
used for the sweep; through the suite runner (which bumps it to 12 g) it passes
in 198 s. It needs more than the 300 s default timeout.

Re-verified after merging `origin/dev` @ `4e0dfa151` (which brought 11 unrelated
`.rs` changes, several in the JIT). Interleaved A/B on the merged tree,
`TestNonBlockingCoordinator`: **clean dev 0/6 PASS (still ~145 s per run, the
original pathological time); dev + these fixes 5/5 PASS at 12–20 s.** Across
every post-merge coordinator run: **18 PASS / 19**.

The one non-PASS was a 400 s stall with
`STW cross-thread JIT takeover is still waiting for cooperative mutators
rounds=64 pending=4 taken=0`, seen exactly once and never reproduced in the 18
runs since. It is in JIT stop-the-world takeover machinery that none of these
changes touch, and if anything cause 3 makes that machinery *less* likely to
stall (a thread parked on the old `parking_lot` mutex was a non-cooperating
mutator outside any blocking region; it no longer parks there at all). It was
not seen on the clean-dev control either, but those runs fail differently and
much more slowly, so that is not a clean comparison. **Left unattributed** —
if it recurs, it needs its own investigation, not a re-open of this doc.

## Residual — NOT this bug, and now also FIXED

`org.apache.catalina.tribes.group.TestGroupChannelSenderConnections`
intermittently SIGSEGVed in `testConnectionLinger`. It was **pre-existing on
clean `origin/dev`** and unaffected by the changes here (an interleaved A/B of
12 runs each gave 2/12 crashes on the baseline binary and 2/12 on the fixed
one), so it was filed separately — and has since been root-caused and fixed on
its own, as two further use-after-frees: the pending `finalize()` queue was not
a GC root, and `HashMap.readObject` held its receiver and its
`ObjectInputStream` in bare Rust locals across GC-capable replay calls. See
[tribes-senderconnections-deserialize-segv-FIXED](tribes-senderconnections-deserialize-segv-FIXED.md).

That file's original attribution — "JIT frames under
`ParallelNioSender.keepalive` → `NioSender.read`" — was **wrong on both
counts**: a crash report's Java frames are published at the last safepoint
deposit rather than at the fault, and its `external/jit` frames are Windows
exception dispatch, not compiled Java. The retired doc explains how to read
these reports correctly.

## Follow-up (latent, same defect class as cause 2)

Three other listen-bind sites still hand a host *string* to `TcpListener::bind`
and so retain the multi-address fallback. None is on the Tribes path, so they
were left alone rather than changed unverified:

- `native-io/src/async_socket.rs::aio_assc_bind` (AsynchronousServerSocketChannel)
- `native-builtins/src/phases_late/net_channels.rs` (plain `ServerSocket`, two sites)
- `native-builtins/src/wildfly_undertow.rs`

## Reproduction (historical)

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref-marking-these-2-non-PASS> -TimeoutSec 300 -Parallel 2 -RunName tribes-repro -Exe <cratonvm.exe>
```
