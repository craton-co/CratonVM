# `InetAddress.getByAddress(byte[])` returns an RFC-5952-compressed IPv6 string instead of HotSpot's uncompressed form — dual/conflicting native registrations

**Status: OPEN — found 2026-07-28**

## Symptom

Affected classes (all from `RunName=craton-rerun-20260728`):

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` | `specificIPAddressNotReverseResolved` |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` | `specificIPAddressWithSslIsNotReverseResolved` |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.reactive.JettyReactiveWebServerFactoryTests` | `specificIPAddressNotReverseResolved` |

All three fail with the identical shape:

```
org.opentest4j.AssertionFailedError:
expected: "fe80:0:0:0:67b0:99e:5a9b:287e"
 but was: "fe80::67b0:99e:5a9b:287e"
       org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests.specificIPAddressNotReverseResolved(JettyServletWebServerFactoryTests.java:483)
```

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard1/logs/module_spring-boot-jetty.org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests.out.log`,
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard1/logs/module_spring-boot-jetty.org.springframework.boot.jetty.reactive.JettyReactiveWebServerFactoryTests.out.log`

The test (`JettyServletWebServerFactoryTests.java:476-484`):

```java
void specificIPAddressNotReverseResolved() throws Exception {
    JettyServletWebServerFactory factory = getFactory();
    InetAddress localhost = InetAddress.getLocalHost();
    factory.setAddress(InetAddress.getByAddress(localhost.getAddress()));
    this.webServer = factory.getWebServer();
    this.webServer.start();
    Connector connector = ((JettyWebServer) this.webServer).getServer().getConnectors()[0];
    assertThat(((ServerConnector) connector).getHost()).isEqualTo(localhost.getHostAddress());
}
```

It compares the string Jetty's connector reports for the address built via
`InetAddress.getByAddress(localhost.getAddress())` against
`localhost.getHostAddress()` (the string from the *original*
`InetAddress.getLocalHost()`). `getLocalHost()`'s string is correctly
formatted (`fe80:0:0:0:67b0:99e:5a9b:287e` — real JDK's uncompressed,
all-eight-groups IPv6 form); `getByAddress()`'s resulting address string is
the RFC-5952-*compressed* form (`fe80::67b0:99e:5a9b:287e`) instead —
the same IP, formatted two different ways by two different code paths in the
same running process.

## Root cause (confirmed at file:line precision)

`java/net/InetAddress.getByAddress([B)Ljava/net/InetAddress;` is registered
**twice** in `native-builtins`:

1. `native-builtins/src/net_phase_e.rs:5039-5059` — builds the IPv6 string
   with `Ipv6Addr::from(octets).to_string()` (line 5053), then passes it
   through `alloc_inet_address()` (`net_phase_e.rs:1643-1649`), which
   canonicalizes via `hotspot_ip_string()` (`net_phase_e.rs:1627-1641`) —
   the helper this same file's own doc comment (lines 1610-1626) explains
   exists *specifically* to avoid Rust's `Ipv6Addr::to_string()` RFC-5952
   zero-compression and instead emit HotSpot's full eight-group
   `Inet6Address.numericToTextFormat` form. **This registration is correct.**

2. `native-builtins/src/net_uri_inet.rs:1456-1482` (`native_inet_get_by_address`,
   registered at `net_uri_inet.rs:216-221`) — builds the IPv6 string the
   same way (`std::net::Ipv6Addr::from(octets).to_string()`, line 1469) but
   **does not** route it through `hotspot_ip_string()`/`alloc_inet_address()`.
   It instead calls `alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2)`
   directly and stores the raw, RFC-5952-compressed string into both the
   `host` and `addr` fields (lines 1476-1481).

Two independent native functions are registered on the exact same
`(class, method, descriptor)` triple. The observed failures — the
*compressed* form winning — mean whichever registration mechanism resolves
duplicate registrations in this build is currently keeping (or later
overwriting with) `net_uri_inet.rs`'s uncanonicalized version rather than
`net_phase_e.rs`'s correct one. This matches the "dead registrations look
alive" pattern already documented for this codebase's registry
(`DatagramChannel` had three competing registries before being unified —
see `reference_datagramchannel_three_registries_unified.md`): the correct
implementation exists and looks like it should apply, but a second,
un-audited registration for the identical method silently wins or interferes
at runtime.

Confirmed via direct source reading (not run, since this task is
log-analysis only — no build/test execution performed): `net_phase_e.rs`'s
`getHostAddress()` (line 4977-4980) simply returns whatever string is
already stored in the `InetAddress` object's `IA_ADDR`/field-1 slot — it
does no formatting itself, so the two different constructors above,
producing two differently-formatted strings, is sufficient by itself to
explain both this bug and the fact that `getLocalHost()` (which goes through
a different construction path, `alloc_inet_address`-based, per
`net_phase_e.rs`'s own header comment) is unaffected.

## Confirming/refuting this hypothesis

Grep-confirmed both registrations exist and target the identical descriptor:

```
$ grep -n '"getByAddress"' native-builtins/src/*.rs
native-builtins/src/net_phase_e.rs:5041:        "getByAddress",
native-builtins/src/net_uri_inet.rs:218:        "getByAddress",
```

To fully confirm which one is *live* in a given build (rather than infer it
from the observed compressed output, which is consistent with
`net_uri_inet.rs`'s version winning), trace the registry's registration
order/overwrite behavior for `java/net/InetAddress.getByAddress`, or add a
temporary `eprintln!` to each implementation and rerun
`specificIPAddressNotReverseResolved`.

## Suggested fix

Remove `native-builtins/src/net_uri_inet.rs`'s `native_inet_get_by_address`
(and its registration at `net_uri_inet.rs:216-221`) so
`net_phase_e.rs`'s canonicalizing implementation is the sole registration —
mirroring how `getHostAddress`/`getHostName`/`getCanonicalHostName` are only
registered once each in `net_phase_e.rs`. Before removing, check whether
`net_uri_inet.rs`'s version differs in some other behavior (e.g. IPv4
handling, or the `alloc_concurrent_synthetic` 2-field layout vs.
`alloc_inet_address`'s concrete-class layout) that some other caller
actually depends on.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.reactive.JettyReactiveWebServerFactoryTests` |
