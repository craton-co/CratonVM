# `ServerSocketChannel` binds loopback for the wildcard, and drops `SO_REUSEADDR`

**Status:** OPEN. Measured 2026-08-10, Windows 11, against Temurin 25.0.3+9 on
the same host in the same minute. Reproduced twice.

This is the answer `probes/ServerSocketPortContentionProbe.java` was written to
collect, and it is not the row anyone expected to be interesting. The
**contention** rows agree between the two VMs exactly. The divergence is on the
probe's own **control** row — a port nothing holds — and it is confined to the
`java.nio` path.

## The paired run

```text
                              HotSpot 25.0.3                    CratonVM
port=8080 (held by an unrelated process)
wildcard reuse=false        BindException                     BindException
wildcard reuse=true         BindException                     BindException
loopback reuse=false        OK /127.0.0.1:8080                OK /127.0.0.1:8080
loopback reuse=true         OK /127.0.0.1:8080                OK /127.0.0.1:8080
control port=18087          OK /0.0.0.0:18087                 OK /0.0.0.0:18087
new ServerSocket(port,50)   BindException                     BindException
channel reuse=default       BindException                     BindException
channel reuse=false         BindException                     BindException
channel reuse=true          BindException                     BindException
channel control 18087       OK true  /[0:0:0:0:0:0:0:0]:18087 OK false /127.0.0.1:18087
```

## Two defects on that last row

The probe's `channelBind` does three things: `setOption(SO_REUSEADDR, true)`,
read it back, then `bind(new InetSocketAddress("0.0.0.0", port), 50)` and print
`getLocalAddress()`.

1. **`SO_REUSEADDR` does not stick.** CratonVM reads back `false` immediately
   after setting `true`. HotSpot reads back `true`. A `setOption` whose paired
   `getOption` disagrees is a silent no-op, and the caller has no way to know.

2. **The wildcard becomes loopback.** `bind("0.0.0.0", p)` reports
   `getLocalAddress() == /127.0.0.1:p` on CratonVM and the wildcard on HotSpot.
   A server that asked to listen on every interface is listening on one, and
   `getLocalAddress()` tells it so — but only if it looks.

Neither is about contention: this row uses a free port, and both VMs bind it
successfully. The bug is in what they bound and with which options.

## Why it is confined to `java.nio`

The `java.net.ServerSocket` control row two lines above answers
`/0.0.0.0:18087` on **both** VMs, from the same `"0.0.0.0"` string. So the
address parses correctly and the `ServerSocket` path is right; only
`ServerSocketChannel` is wrong.

That is exactly the split the probe's own comment predicted:

> Tomcat's NioEndpoint and Jetty's ServerConnector do NOT use
> `java.net.ServerSocket` — they bind a `java.nio` `ServerSocketChannel`, and
> the two APIs do not behave the same way on Windows. […] Measuring only the
> ServerSocket rows and generalising is how a bind divergence hides.

## Why this is a candidate for the embedded-container cluster

An embedded Tomcat or Jetty binds through `ServerSocketChannel`. Under CratonVM
it would come up on loopback while believing it is on every interface, and its
`SO_REUSEADDR` would be dropped. Any test that connects from a non-loopback
address, or that restarts a container on the same port within `TIME_WAIT`, has
a mechanism here. That is a hypothesis with a measurement behind it, not a
diagnosis — it has not been traced to a specific failing Spring Boot class.

## Not fixed here

Found while verifying an unrelated branch
(`fix/build-residuals-20260809`); recorded rather than fixed because the bind
path is its own change with its own verification. The reproducer is
`probes/ServerSocketPortContentionProbe.java`, already on `dev`; the control row
needs no occupant, so it reproduces on any host.
