# `SSLEngine.toString()` throws on a CratonVM-allocated engine

**Status: FIXED 2026-08-21**, branch `fix/netty-tls-residuals-20260821`. Found
2026-08-20 with `probes/SslContextSpiProbe.java` while validating the
`sslcontext-natives-ignore-a-third-party-spi` fix. Filed rather than folded into
it: that page is about `javax.net.ssl.SSLContext` answering for somebody else's
SPI, and this is `javax.net.ssl.SSLEngine` answering for CratonVM's own object.

```
HotSpot 25  SSLEngine[hostname=null, port=-1, Session(1787281750249|SSL_NULL_WITH_NULL_NULL)]
before      THREW java.lang.NullPointerException: Cannot read field "conSession"
after       SSLEngine[hostname=null, port=-1, Session(1787281754173|SSL_NULL_WITH_NULL_NULL)]
```

## What it took, and the trade-off that turned out not to exist

The page asked whether `toString()` should report the side-table host or the
`peerHost`/`peerPort` fields, "and only one of them is useful in a log". They do
not differ: `set_engine_peer_host` already writes both, so the JDK's own
accessors are right for `createSSLEngine(host, port)`.

What was actually wrong is the OTHER overload. `javax.net.ssl.SSLEngine`
declares `private int peerPort = -1` and the no-arg `SSLEngine()` leaves it
there; a synthetic allocation leaves the slot at its untagged **0** — a
legal-looking port number nobody asked for, which `toString()` then prints.
`createSSLEngine()` now seeds -1, and both overloads match HotSpot.

Three registrations, and the third is why the shape is not duplicated:

* `toString()` on `sun/security/ssl/SSLEngineImpl`, rebuilt from
  `getPeerHost()` / `getPeerPort()` / `getSession()`.
* the `peerPort = -1` seed above.
* `toString()` on `javax/net/ssl/SSLSession` — the class CratonVM's session
  objects actually carry — so JSSE's `Session(creationTime|cipherSuite)` shape
  lives in ONE place and `String.valueOf(session)` is right on its own rather
  than printing an identity hash.

**The sibling the page asked about is not reachable.**
`sun.security.ssl.SSLSocketImpl.toString()` has the identical `conContext`
dereference, but CratonVM allocates its SSL sockets as
`javax/net/ssl/SSLSocket` — never as `SSLSocketImpl` — so that body has no
receiver here. Checked rather than assumed: `try_alloc_concurrent_synthetic`
is called with `"javax/net/ssl/SSLSocket"` at every site.

## What it is

`SSLContext.createSSLEngine()` on CratonVM allocates a
`sun.security.ssl.SSLEngineImpl` and populates the one JDK-visible invariant its
natives need (`engineLock`). `toString()` is NOT one of the methods CratonVM
registers, so the real JDK body runs — and JDK 25's is:

```java
return "SSLEngine[hostname=" + getPeerHost() +
       ", port=" + getPeerPort() +
       ", " + conContext.conSession + "]";
```

`conContext` is a `sun.security.ssl.TransportContext` that only SunJSSE's own
constructor chain creates, and CratonVM never runs it. Measured on JDK 25, same
host, same classpath, `SSLContext.getInstance("TLSv1.3")` then `init` then
`createSSLEngine()`:

```
                    HotSpot 25                                       CratonVM
String.valueOf(e)   SSLEngine[hostname=null, port=-1,                THREW java.lang.NullPointerException:
                    Session(1787242252075|SSL_NULL_WITH_NULL_NULL)]    Cannot read field "conSession"
                                                                       because "this.conContext" is null
```

`String.valueOf(engine)` is not an exotic call: any log statement, assertion
message, or `IllegalStateException("… " + engine)` that mentions an engine hits
it, and it throws from inside the failure path rather than from the code under
test.

## Why it is not simply "register toString"

It is close to that, but the answer has to come from somewhere. CratonVM's
engine keeps its peer host and port in a side table
(`t27_tls::set_engine_peer_host`) rather than in `SSLEngine`'s own `peerHost` /
`peerPort` fields, which the JDK's `getPeerHost()` / `getPeerPort()` read
directly — so a registration that just calls those two answers `null` / `0`
where HotSpot answers `null` / `-1` for the no-arg overload and the real host
for `createSSLEngine(host, port)`.

The session half is `SSLSessionImpl.toString()`,
`"Session(" + creationTime + "|" + getCipherSuite() + ")"`. CratonVM's session
object answers `getCreationTime()` and `getCipherSuite()`, so the string is
reconstructible; what has to be decided is whether `SSLEngine.toString()` should
report the side-table host (matching what the engine will actually dial) or the
field (matching what the JDK would print). They differ, and only one of them is
useful in a log.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
./gen-openssl-args.sh --bc18 -o /tmp/bc18.args
javac -cp "$(sed -n 2p /tmp/bc18.args)" -d . <repo>/probes/SslContextSpiProbe.java
java @/tmp/bc18.args SslContextSpiProbe | grep engineToString
<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1500m \
    @/tmp/bc18.args SslContextSpiProbe | grep engineToString
```

## Related

- the retired `sslcontext-natives-ignore-a-third-party-spi` write-up — §5 lists
  this among the rows that separate the CratonVM-owned arm from HotSpot, and
  records that this work did not move any of them.
