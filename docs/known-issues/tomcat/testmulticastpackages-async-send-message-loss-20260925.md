# `TestMulticastPackages.testDataSendASYNCM` — CratonVM drops ~1-4% of multicast messages under async burst load

| | |
|---|---|
| **Status** | **PARTLY FIXED, rest OPEN** (2026-10-05). Root cause found: the datagram natives paid a 14-45 us GC-blocking bracket per datagram ([fixed, doc 51](../../internal/fixed-suite-bugs/tomcat/51-blocking-region-bracket-around-calls-that-cannot-block-udp-receive-30x-hotspot-FIXED-20261005.md)), so the single receiver thread per channel fell behind and the 64 KiB socket buffer overflowed. After the fix the test passes 5 of 8 standalone runs on `cratonvm-tomcatjitonly-g1-22.exe` (before: about 1 in 4 under load) and the remaining failures lose 9-27 of 10,000 messages. What is left: the receiver thread stalls for 10-150 ms inside its processing step in some runs (see "Still open" below). |
| **HotSpot** | PASS (`OK (5 tests)`) — 0 message loss across the run (9/9 probe and test runs, loaded and idle) |
| **CratonVM** | g1-22: `OK` 5/8, otherwise `Tests run: 5, Failures: 1` with 9-27 lost. g1-17 and earlier: FAIL, 100% of 4 runs when first measured, 1% to 7% lost. |
| **Discovered** | 2026-09-25, re-verifying the [`nonpassed-classbyclass-census.md`](nonpassed-classbyclass-census.md)'s "Tribes multicast — already a known environmental gap" grouping |

## This one is not the environmental gap the other two are

The census grouped `TestMulticastPackages` with `TestRemoteProcessException`
and `TestUdpPackages` as "known environmental" (this host's multicast
networking doesn't work, HotSpot fails identically). Re-verified all three
against a fresh same-fixture HotSpot control:

| Class | HotSpot | CratonVM |
|---|---|---|
| `TestMulticastPackages` | `OK (5 tests)` | **`Tests run: 5, Failures: 1`** |
| `TestRemoteProcessException` | `Tests run: 1, Failures: 1` | `Tests run: 1, Failures: 1` (matches) |
| `TestUdpPackages` | `Tests run: 6, Failures: 6` | `Tests run: 6, Failures: 6` (matches) |

The other two are still genuinely environmental — HotSpot fails them
identically on this host's network. `TestMulticastPackages` is not: HotSpot
passes cleanly, so this is a real CratonVM-only divergence that had been
miscategorized by association with its two siblings.

## Symptom

```
1) testDataSendASYNCM(org.apache.catalina.tribes.test.channel.TestMulticastPackages)
java.lang.AssertionError: Checking success messages. expected:<10000> but was:<9686>
```

`testDataSendASYNCM` fires 10,000 multicast messages across multiple sender
threads (`threadCount`, `NO_ACK` mode) and counts how many the receiver's
`MessageListener` observed. Reproduced 4 times total (1 initial + 3 reruns),
**every single run loses some messages**, but the exact count is not fixed:

```
run 1 (initial): 9686 / 10000 (314 lost, 3.14%)
run 2:           9898 / 10000 (102 lost, 1.02%)
run 3:           9631 / 10000 (369 lost, 3.69%)
run 4:           9741 / 10000 (259 lost, 2.59%)
```

Not a flake in the sense of "sometimes passes" — it never passes — but the
loss magnitude is timing-dependent, consistent with a capacity/backpressure
race rather than a deterministic off-by-N counting bug.

## What was checked

The test explicitly raises both UDP buffer sizes to 10 MiB before the burst
(`ReceiverBase.setUdpRxBufSize`/`setUdpTxBufSize(1024*1024*10)`), anticipating
exactly this kind of OS-receive-buffer-overflow-under-burst failure mode.
Traced the setting through to `native-api/src/fd_table.rs`'s
`udp_set_recv_buffer_size`/`udp_set_send_buffer_size`: both genuinely call
`socket2::SockRef::set_recv_buffer_size`/`set_send_buffer_size` — a real
`setsockopt(SO_RCVBUF/SO_SNDBUF)` — not a value stored and silently dropped.
So the buffer-size request itself is not being ignored at the VM layer; if
it is not taking effect, that would be a Windows OS-level cap rather than a
CratonVM code path, and would affect HotSpot's socket identically since both
JVMs make the same OS call on the same host. Since HotSpot loses **zero**
messages on the identical run, the buffer-size ceiling (if any) is not the
differentiator.

## Root cause (2026-10-05): the multicast datagram path costs 30x HotSpot per receive, and the 64 KiB socket buffer overflows

**Earlier sections of this page that said the data travels over TCP / `NioReceiver` were wrong.**
`SEND_OPTIONS_MULTICAST` makes `ChannelCoordinator.sendMessage` call `membershipService.broadcast(msg)`,
so every one of the test's 10,000 messages is a real UDP multicast datagram, sent by
`McastServiceImpl.send` and read by ONE `Tribes-MembershipReceiver` thread per channel
(`McastServiceImpl.receive` -> `MulticastSocket.receive` with the OS-default 64 KiB receive buffer, then an
executor hop per datagram). The `setUdpRxBufSize(10 MiB)` the test sets is for `NioReceiver`'s datagram
channel, after it has already been bound; it never reaches the multicast socket on either VM.

Counting at each hop (shadow copies of `McastServiceImpl`, `McStats` counters; run on `g1-17`, JIT on):

| hop | count over 10,000 sends (2 receivers = 20,000) |
|---|---|
| sender `completions` (async dispatch returned) | 10,000 |
| `socket.receive` returned (both channels) | 19,956 .. 20,178, i.e. short by 0-44 in the runs that lose messages |
| broadcast packages parsed / executor hops | equal to the line above |
| `messageReceived` delivered to the listener | 9,980 in the run that lost 20 |

So the loss is **datagrams that never come out of `socket.receive`**: the receiver thread falls behind the
sender for a few ms and the kernel drops the overflow (netstat's "receive errors" stays 0; it does not count
this). With Tribes' trace logging on, the send side slows and nothing is lost (0/8 runs), so the loss is purely
a throughput race. HotSpot: 0 lost in 5/5 probe runs and 4/4 test runs under the same machine load.

What makes the receiver slow, measured with a 600-byte datagram already queued (no waiting involved):

| call | `g1-17` | HotSpot |
|---|---|---|
| `DatagramChannel.receive` (direct 64K / 2K / heap 2K buffer) | 78-91 us | 1.5-2.0 us |
| `DatagramSocket.receive` | 82-88 us | 1.9 us |
| `MulticastSocket.receive` (saturated, unicast blast from another process) | 51-63 us | ~2 us |
| non-blocking `SocketChannel.read`, no data | 14-15 us | 0.45 us |

The cost does not depend on the payload size (1 byte: 49-65 us) and is not the OS: the fixed part is the
**GC-blocking bracket** (`begin_blocking_region` = TLAB retire + a full root-snapshot deposit,
`end_blocking_region` = post-block re-sync, ~45 us together) that every datagram native wraps around a
`recv` that has a datagram waiting and could not have blocked. The same bracket wraps `sendto` and every
non-blocking `SocketChannel` read/write. Two smaller costs sat on top: per-byte `ByteBuffer` stores for the
payload (`bb_write_byte` in a loop) and `getData`/`setLength`/`setPort` upcalls per datagram.

### Fix (this page's status: see the table at the top)

* `FileDescriptorTable::udp_recv_if_ready` / `udp_send_if_ready` (`native-api/src/fd_table.rs`): a zero-timeout
  `poll` first; a ready socket does the `recv_from`/`send_to` OUTSIDE the bracket, an empty one falls through
  to the unchanged bracketed wait (so close-awareness and `SO_TIMEOUT` are untouched). Used by
  `native_dc_receive` / `native_dc_read` / the scattering read / `native_dc_send` (`native-io/src/lib.rs`),
  the `DatagramSocket` natives (`native-builtins/src/net_phase_e.rs`) and `MulticastSocket.receive`/`send`
  (`native-io/src/net.rs`).
* `bb_write_bytes` copies a heap buffer in one `write_byte_array_from` instead of one `set_array_element` per
  byte; the datagram reads use it.
* `SocketChannel` read, write, gathering write and scattering read skip the bracket when the channel is
  non-blocking (its OS socket is non-blocking too, so the call cannot park).
* `MulticastSocket.receive` reuses a per-thread receive buffer (no 64 KiB zero-fill per datagram) and stores
  `length`/`port` and reads `buf`/`offset`/`length`/`port` directly instead of calling the accessors.

Measured after (`g1-18`/`g1-19`): `DatagramChannel.receive` 12-19 us, non-blocking read with no data 1.2 us,
saturated `MulticastSocket.receive` 51-63 -> 28-38 us. See the status table for the test result.

> **After merging `origin/dev` (2026-10-06) this class got much worse for a reason outside this page**: the dev tip alone lost 22-30% of the burst, the merge 37-48%, the later tip more than half. Cause and fix: [doc 52](../../internal/fixed-suite-bugs/tomcat/52-initiating-memo-rewrite-bumped-the-resolution-epoch-on-every-loadclass-drive-multicast-burst-lost-half-FIXED-20261006.md) (a re-recorded initiating answer bumped the resolution epoch); with it the class is back at `g1-22`'s level (9,624-9,836 of 10,000 received on `g1-27`). The numbers below are from the pre-merge builds (`g1-17`..`g1-22`).

## Still open (2026-10-05)

With the receive path at 3 us per datagram (HotSpot 1.7) a few runs still lose 0.1-0.3%. A shadow copy of `McastServiceImpl.receive` timing the processing step after `socket.receive` (parse, `executor.execute`) logs stalls of 1.5-15 ms early in the run (cold Java) and, in some runs, **10-160 ms in the middle of the burst, hitting both channels' receivers at the same moment**. They occur under the default ZGC, Serial, G1 and a 4-12 GiB heap alike, with no collection logged and `GarbageCollectorMXBean` reporting 0, so they are not a collection. The utility executor that receives each datagram's task is `ScheduledThreadPoolExecutor(1)` (one worker for all deserialization and delivery); a JIT code install, a class load or a monitor handoff in that path would all look like this. The sampler (`tools/psample`, per-thread view) shows the receiver thread about half busy in interpreter/C1 frames (`XByteBuffer.firstIndexOf` byte loops, `execute_frame_from_index`) and no single hot spot. Not identified.

## Update 2026-10-14: the test was red for another reason on the `dev` tips with the Windows channel layer, and is back to the loss

On the `dev` tip `5df605a929` (`--jdk-only`, Windows, layer default on) `TestMulticastPackages` failed 5 of 5 tests, none of them on loss: `ChannelException: java.lang.IllegalArgumentException: IPv6 socket cannot join IPv4 multicast group` from `MulticastSocket.joinGroup` (`Net.canIPv6SocketJoinIPv4Group0` answered `false` on Windows), so no member started. Fixed in [doc 56](../../internal/fixed-suite-bugs/tomcat/56-multicastsocket-joingroup-ipv4-failed-on-windows-ipv6-socket-cannot-join-ipv4-group-testmulticastpackages-5-of-5-red-FIXED-20261014.md). With it the test runs, and the loss is back: four runs (ZGC, JIT on, a host with other sessions' builds running), `testDataSendASYNCM` **9,625 / 9,599 / 9,573 / 9,676 of 10,000** in 4 of 4, and `testDataSendACK` **499 of 500** in 3 of 4 (the shortfall of one in the ACK mode is new to this page). HotSpot, same host and harness: `OK (5 tests)`. The datagram path this page measured (hop counts above) was the native one the Windows layer replaced with the JDK's own `DatagramChannelImpl`, so the hop table is not re-taken on the new path; the open item stands as 3-4 % loss under the burst. The three other Tribes classes of the family: `TestDataIntegrity` 2 of 5 fail at 9,999 of 10,000 (HotSpot here: 3 of 5 at 9,987-9,995), `TestUdpPackages` 6 of 6 fail on both VMs.

## Also found on this path

`MulticastSocket` accessors that disagreed with HotSpot: the 2026-10-05 page was retired as [doc 56](../../internal/fixed-suite-bugs/tomcat/56-multicastsocket-joingroup-ipv4-failed-on-windows-ipv6-socket-cannot-join-ipv4-group-testmulticastpackages-5-of-5-red-FIXED-20261014.md) (the accessors were already right on the Windows channel layer; what was wrong was `joinGroup`, below).

## Reproduce

```powershell
cd apps\tomcat-suite-runner
.\run-one.ps1 -Vm hotspot -Class org.apache.catalina.tribes.test.channel.TestMulticastPackages
.\run-one.ps1 -Vm craton -Exe <cratonvm.exe> -Class org.apache.catalina.tribes.test.channel.TestMulticastPackages
```
