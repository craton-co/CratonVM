# DatagramChannel probes

Two standalone programs for the `java.nio.channels.DatagramChannel` path.
Run each against HotSpot first and diff — both should match line for line
(modulo `InetSocketAddress.toString`'s leading `/` and the impl class name,
which is CratonVM's synthetic channel rather than `sun.nio.ch.DatagramChannelImpl`).

```powershell
$JH = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$JH\bin\javac" -d tools\udp-probe tools\udp-probe\*.java
& "$JH\bin\java"  -cp tools\udp-probe UdpPathProbe
& <cratonvm.exe>  -cp tools\udp-probe UdpPathProbe
```

* **`UdpPathProbe.java`** — open → bind → send → receive, then a multicast
  `join`/`drop`. This is the acceptance test for the registry unification
  (dev, 2026-07-28): before it, CratonVM failed `send` and `join` with
  `IOException: no socket id` and returned `null` from `receive`, because
  `open()`/`bind()` populated `ctx.fd_table()` while `send`/`receive`/`join`
  looked in a private registry in `native-io/src/datagram.rs` that nothing
  ever wrote to.

* **`UdpWedgeProbe.java`** — parks one channel in a blocking `receive()` with
  nothing ever sent to it, then times an unrelated open/bind/close on a second
  channel. Guards the invariant that a parked receive must not hold the UDP
  registry lock across the syscall. Meaningful only once the channels share one
  registry, which is why it landed alongside the unification.
