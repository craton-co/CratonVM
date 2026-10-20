# Unix-domain-socket probes

Two standalone Java programs used to develop and verify CratonVM's AF_UNIX
channel support (see
`27-xxxendpoint-unix-domain-socket-init-failure-FIXED.md`).
Both print `OK`/`FAIL` per step and are meant to be diffed against HotSpot.

* **`UdsProbe.java`** — the prerequisites and the basic round trip:
  `Path.of`, `FileSystems.getDefault()` identity, `Class.getModule()` identity,
  `UnixDomainSocketAddress.of`, `{Server,}SocketChannel.open(UNIX)`, then
  bind → accept → read/write → close between a server thread and a client.

* **`UdsSelectorProbe.java`** — the Tomcat `NioEndpoint` topology: a blocking
  acceptor thread hands each accepted channel to a `Selector`-driven poller
  that reads the request and writes the response. Catches selector-registration
  gaps that the basic round trip does not.

```powershell
$JH = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$JH\bin\javac" -d tools\uds-probe tools\uds-probe\*.java
& "$JH\bin\java"  -cp tools\uds-probe UdsProbe          # HotSpot baseline
& <cratonvm.exe>  -cp tools\uds-probe UdsProbe          # compare
```
