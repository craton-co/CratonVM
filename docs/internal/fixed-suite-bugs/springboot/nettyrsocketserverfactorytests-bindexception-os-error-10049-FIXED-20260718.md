# RSocket Netty transport BindException / WSAEADDRNOTAVAIL (fixed 2026-07-18)

## Symptom

`org.springframework.boot.rsocket.netty.NettyRSocketServerFactoryTests` had
20 failures out of 21 on Windows. Netty's client connected to the address
published by the just-started embedded RSocket server and Windows rejected the
wildcard destination with `WSAEADDRNOTAVAIL` (os error 10049).

## Root cause and fix

`native-io/src/socket_channel.rs::ssc_local_address` published a wildcard
listener (`0.0.0.0` or `::`) verbatim. Binding a wildcard listener is valid,
but it is not a valid local client destination on Windows. The VM now publishes
the corresponding loopback address (`127.0.0.1` or `::1`) for wildcard
listeners while preserving concrete listener addresses.

Removing that connect failure exposed two real JSSE bridge residuals in the
same test class:

- synthetic `SSLEngineImpl` instances lacked the real JDK `engineLock`, causing
  Netty ALPN setup to throw an NPE;
- the generic `TrustManagerFactory.getTrustManagers` native replaced a concrete
  provider factory, including Netty's `InsecureTrustManagerFactory`, with a
  default PKIX manager.

The SSL engine allocation now installs a constructed `ReentrantLock` in
`engineLock`. The factory native is restricted to Craton's synthetic default
factory; concrete provider factories execute the real Java factory/SPI path.
The rustls engine continues to defer configured Java trust-manager policy until
after cryptographic handshake completion.

## Validation

- HotSpot baseline: 21/21 tests pass.
- CratonVM release executable built from this change: 21/21 tests pass with
  JIT enabled (9.1 seconds).
- The same executable: 21/21 tests pass with JIT disabled (7.9 seconds).
- Focused native I/O regression test verifies wildcard listener publication
  converts only wildcard addresses.

The JIT trace confirmed the client connect target is `127.0.0.1:<ephemeral>`
and succeeds immediately; no `WSAEADDRNOTAVAIL` remains.
