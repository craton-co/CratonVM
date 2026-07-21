# WildFly management connection fails `JBOSS-LOCAL-USER` SASL — the dominant suite blocker once the container boots

Status: **OPEN — new, found 2026-07-21.** Newly dominant WildFly integration-suite failure, exposed
by the six boot fixes in
`docs/internal/fixed-suite-bugs/wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md` (2026-07-21
seventh session). Before those fixes the container never finished booting, so no test ever reached
management authentication; now the container boots to a fully functional management endpoint and
**every** Arquillian class fails at the next step instead — the management client cannot authenticate.

## Symptom

Arquillian reports `LifecycleException: Could not start container`, root-caused (in the client-side
surefire `*-output.txt`) to:

```
Caused by: java.util.concurrent.TimeoutException: Managed server was not started within [60] s
```

which is downstream of a `JBOSS-LOCAL-USER` SASL rejection on the management remoting connection. The
client-side remoting trace shows:

```
TRACE [org.wildfly.security] Created SaslClient for mechanism JBOSS-LOCAL-USER
TRACE [org.jboss.remoting.remote.client] Client initiating authentication using mechanism JBOSS-LOCAL-USER
TRACE [org.jboss.remoting.remote.client] Client received authentication challenge
TRACE [org.wildfly.security.sasl.local] SASL Negotiation Completed          <-- client is happy
TRACE [org.jboss.remoting.remote.client] Client sending authentication response
DEBUG [org.jboss.remoting.remote.client] Client received authentication rejected for mechanism JBOSS-LOCAL-USER
    javax.security.sasl.SaslException: JBOSS-LOCAL-USER: Server rejected authentication
```

`JBOSS-LOCAL-USER` is a filesystem-proof-of-locality mechanism: the server writes random challenge
bytes to a temp file, tells the client the path, the client reads the bytes and echoes them, the
server compares. The client says "Negotiation Completed" (it read the file and computed a response),
then the **server rejects** the echoed response — i.e. the mismatch is on the server's comparison, or
the challenge bytes were corrupted somewhere between write and read-back over the wire.

## The container is genuinely healthy — this is NOT a boot problem

Direct standalone boot (exact Arquillian-captured cmdline, JIT on) reaches a bound management port in
~15 s and serves HTTP correctly:

```
$ curl -s -i http://127.0.0.1:9990/
HTTP/1.1 302 Found
Location: /console/index.html

$ printf 'GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: jboss-remoting\r\nConnection: upgrade\r\nSec-JbossRemoting-Key: AAAA\r\n\r\n' | nc 127.0.0.1 9990
HTTP/1.1 101 Switching Protocols
Connection: Upgrade
Upgrade: jboss-remoting
Sec-JbossRemoting-Accept: +nklLI0MxWJg1GvsoHVTdlGNpcs=
```

So HTTP routing, the console redirect, and the `jboss-remoting` HTTP-upgrade handshake (including the
correct `Sec-JbossRemoting-Accept` = base64(SHA-1(key + magic))) all work. The failure is in the
jboss-remoting protocol that runs over the raw socket **after** the upgrade.

## Localization: server-side remoting/SASL, not the client

- **Real-JDK `jboss-cli-client.jar` against the (healthy) CratonVM container**: times out during the
  post-upgrade remoting handshake — `java.net.ConnectException: WFLYPRT0023: Could not connect to
  remote+http://127.0.0.1:9990. The connection timed out`. A fully-real client cannot even complete
  the remoting negotiation, which points at the CratonVM container's remoting stream diverging from
  the wire protocol after the upgrade.
- **CratonVM client (the surefire JVM) against the CratonVM container**: gets further — completes the
  upgrade, exchanges the SASL challenge, and is rejected. Two CratonVM peers "agree" enough to reach
  SASL but still disagree on the challenge bytes.

Conclusion: the defect is in CratonVM's **management remoting layer** (post-upgrade jboss-remoting
framing over the XNIO conduits) and/or the **Elytron `LocalUser` server-side challenge handling** —
not in the client and not in boot.

## The primitives underneath are sound (ruled out)

MicroProbes under the same fix9 binary:
- File byte roundtrip (write 0..255, read back, `Files.readAllBytes`): `ROUNDTRIP-OK`, `NIO-OK`.
- Blocking-socket loopback echo, 20 rounds of random 1–4096-byte payloads: `total=40752 bad=0
  SOCK-OK`.

So plain file and socket I/O carry bytes losslessly. The corruption/mismatch is specific to the
remoting/SASL framing, not to the byte pipes it runs on.

## Where to start

1. `native-builtins/src/xnio_conduits.rs` has a built-in TCP tracer: `CRATONVM_DBG_XNIO_TCP=1` prints
   `[cratonvm:xnio-tcp]` with byte previews for every conduit read/write. Run a boot + a single
   real-JDK `jboss-cli-client.jar` connect under it and diff the on-wire remoting frames against a
   real-JDK-container capture (`tcpdump -i lo -A port 9990` works and was used to confirm the HTTP
   upgrade request bytes are correct).
2. `org.wildfly.security.sasl.localuser.LocalUserServer` (server) vs `LocalUserClient` (client) — the
   challenge is `SecureRandom` bytes written to a temp file under `jboss.server.temp.dir`; verify
   both processes resolve the SAME path and that the bytes read back equal the bytes written (a
   `CRATONVM_DBG_STALE_OBJREF`-style probe on the challenge byte[] would catch a stale-array bug).
3. A useful bisection: whether a real-JDK container + CratonVM client authenticates (isolates the
   client half) — this doc only tested real-client/cvm-server and cvm/cvm.

## Related

`docs/internal/fixed-suite-bugs/wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md` — the boot
hang whose fix exposed this. Numerous XNIO conduit / jboss-remoting fixes already exist in the
codebase (see the memory index's "HTTP / networking" section) — this is the same subsystem, next
layer up.
