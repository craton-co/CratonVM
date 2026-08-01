# Jetty keeps accepting connections after graceful shutdown begins

**Status: OPEN.** Re-filed 2026-08-01 from
`springboot-basicerrorcontroller-checkcast-abort-20260731.md`, which was
retired that day. Nothing about this residual was fixed by that retirement — it
is filed on its own because it never belonged to the comparator/GC cluster in
the first place, and would otherwise have been buried in an internal FIXED
document.

## Symptom

`module/spring-boot-jetty` ·
`org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` ·
`whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade`

A new connection attempt made after Jetty's graceful shutdown has started is
routed to a handler and answered with `404 Not Found`, where the test requires
it to be refused at the TCP level:

```
java.lang.AssertionError:
Expecting actual:
  404 Not Found HTTP/1.1
to be an instance of:
  org.apache.hc.client5.http.HttpHostConnectException
but was instance of:
  org.apache.hc.client5.http.impl.classic.CloseableHttpResponse
     org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests
       .whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade(JettyServletWebServerFactoryTests.java:337)
```

## What is and is not known

Observed 2026-07-31 in `craton-hangverify-20260731`/`all-jit`. It is
**independent of** the `CaseInsensitiveComparator` / collection-overlay GC
family it was originally filed alongside: no correlated fatal event appears in
the `.err.log` around this test, and the path involves no comparator, no
`TreeSet`/`TreeMap`, and no `checkcast`.

The shape suggests the server's listening socket / connector is not closed at
the point graceful shutdown begins, so new connections are still accepted and
dispatched. **Not investigated at the source level.** Nobody has yet
established whether this is a CratonVM defect at all — a HotSpot control run of
this single test on the same fixture is the first thing to do, and it has not
been done.

No existing document covers this assertion shape (checked
`jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals-FIXED.md`,
`tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED.md`,
`jetty-private-lambda-wrong-receiver-startcontext-recursion-cluster-FIXED.md`).

## Reproduction rate, 2026-08-01

Reproduced on binaries from `fix/springboot-conditionreport-cce-20260801`,
real JDK 25, complete Spring Boot 4.1.0-SNAPSHOT fixture, JIT on: **1 failure
in 4 full-class runs.** The other three pass this test, so a single green run
of the class does not clear it — budget at least 8 runs before calling it
fixed.

The rest of the class is clean as of that binary. The
`localeCharsetMappingsAreConfigured` failure that used to accompany this one
was a different defect (`Locale.toString()` returning `""`, so Jetty's
locale→encoding map collapsed onto one key) and is fixed in `6c2a8a677d`.
HotSpot on the same fixture is 113/113.

## Reproduction

```bash
/data/sbrun.sh <exe> jit module/spring-boot-jetty \
  org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests <outdir> 5 1800
```

Expect roughly one failure per five runs; run the HotSpot arm
(`/data/hsrun.sh` with the same two arguments) first.
