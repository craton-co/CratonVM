# `SSLEngine.toString()` throws on a CratonVM-allocated engine

**Status: OPEN.** Found 2026-08-20 on the Azure Linux host with
`probes/SslContextSpiProbe.java`, while validating the
`sslcontext-natives-ignore-a-third-party-spi` fix. Filed rather than folded into
it: that page is about `javax.net.ssl.SSLContext` answering for somebody else's
SPI, and this is `javax.net.ssl.SSLEngine` answering for CratonVM's own object.

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

## What it would take to close

- Register `toString()` on `sun/security/ssl/SSLEngineImpl`, building the JDK's
  exact shape from the engine's own accessors rather than from `conContext`.
- Decide the host/port question above, and pin it with a test that covers BOTH
  `createSSLEngine()` and `createSSLEngine(host, port)` — the two differ in
  HotSpot's output and a test on only the first would not notice.
- Check the sibling: `sun.security.ssl.SSLSocketImpl.toString()` has the same
  `conContext` dereference, and CratonVM allocates those too.

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
