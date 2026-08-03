# `TestStartupIPv6Connectors.testIPv6MappedIPv4` — connect to `::ffff:127.0.0.1` fails with WSAEADDRNOTAVAIL

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | medium — single, narrow test method |
| **HotSpot** | PASS (4/4, fresh-verified 2026-08-03) |
| **CratonVM** | FAIL (`testIPv6MappedIPv4` only; other 3 methods in the class pass) |
| **Discovered** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |

## Symptom

```
1) testIPv6MappedIPv4(org.apache.catalina.startup.TestStartupIPv6Connectors)
java.io.IOException: HttpURLConnection response failed: connect [::ffff:127.0.0.1]:54718:
Требуемый адрес для своего контекста неверен. (os error 10049)
	at org.apache.catalina.startup.TestStartupIPv6Connectors.assertHttpOkOnAddress(TestStartupIPv6Connectors.java:144)
	at org.apache.catalina.startup.TestStartupIPv6Connectors.testIPv6MappedIPv4(TestStartupIPv6Connectors.java:62)
```

`os error 10049` is Windows `WSAEADDRNOTAVAIL` ("the requested address is not
valid in its context"). The test connects to the IPv4-mapped IPv6 loopback
address `::ffff:127.0.0.1`, which HotSpot's socket stack resolves and
connects to without issue — this is a standard IPv4-mapped-IPv6 address, not
an exotic one.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1; $env:CRATONVM_ROOTSNAP_CACHE=1
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.catalina.startup.TestStartupIPv6Connectors
```

HotSpot control: `OK (4 tests)` in 1.4s.

## Suspected root cause (not yet isolated)

CratonVM's `native-io` socket-channel layer likely does not translate an
IPv4-mapped IPv6 destination address into the correct low-level connect call
on Windows (e.g. failing to set the socket's dual-stack/`IPV6_V6ONLY` option,
or mishandling the mapped-address form when building the `sockaddr`). Not yet
checked against `native-io/src/socket_channel.rs`. No prior known-issue doc
covers this (checked `docs/internal/fixed-suite-bugs` and
`docs/known-issues` — no hits).
