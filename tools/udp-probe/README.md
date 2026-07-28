# DatagramChannel probes

Two standalone programs for the `java.nio.channels.DatagramChannel` path.
Run each against HotSpot first and diff.

```powershell
$JH = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$JH\bin\javac" -d tools\udp-probe tools\udp-probe\*.java
& "$JH\bin\java"  -cp tools\udp-probe UdpPathProbe
& <cratonvm.exe>  -cp tools\udp-probe UdpPathProbe
```

* **`UdpPathProbe.java`** — the minimal open → bind → send → receive round
  trip. As of 2026-07-28 CratonVM fails it in real-JDK mode: `send` throws
  `IOException: send: no socket id` and `receive` returns `null` having read
  nothing, where HotSpot round-trips. Cause is the registry split documented at
  the top of `native-io/src/datagram.rs`: `open()`/`bind()` are served by a
  different native family than `send()`/`receive()`, so the channel's slot-4
  socket id never resolves in `datagram.rs`'s own registry. That module's doc
  comment already flags unifying the two registries as a follow-up.

* **`UdpWedgeProbe.java`** — parks one channel in a blocking `receive()` with
  nothing ever sent to it, then times an unrelated open/bind/close on a second
  channel. Guards the invariant that a parked receive must not hold the UDP
  registry lock (see `dgram_socket` in `native-io/src/datagram.rs`). It cannot
  currently reach the `send`/`receive` natives end-to-end for the reason above,
  so the Rust-level equivalent —
  `datagram::tests::wp37_parked_receive_does_not_block_the_registry` — is the
  real regression guard; this probe becomes meaningful once the registries are
  unified.
