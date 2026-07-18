# `InetAddressFilterTests.whenNull()`: default-method null-check skipped, no exception thrown

**Status: OPEN — found 2026-07-17 (hypothesis, not confirmed to file:line)**

## Symptom

```
JUnit Jupiter:InetAddressFilterTests:MatchesSocketAddressTests:whenNull()
    => java.lang.AssertionError:
Expecting code to raise a throwable.
       org.springframework.boot.http.client.InetAddressFilterTests$MatchesSocketAddressTests.whenNull(InetAddressFilterTests.java:65)
```

1 of 76 tests in the class. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-client.org.springframework.boot.http.client.InetAddressFilterTests.out.log`

## What the test does (confirmed from source)

`InetAddressFilterTests.java:63-67`:

```java
InetAddressFilter filter = (address) -> address != null;   // implements matches(InetAddress)
assertThatIllegalArgumentException().isThrownBy(() -> filter.matches((InetSocketAddress) null))
    .withMessage("'address' must not be null");
```

`InetAddressFilter` declares a **default** method `matches(InetSocketAddress)`
(does `Assert.notNull(...)` then delegates to the abstract
`matches(InetAddress)`) alongside an unrelated **abstract**
`matches(InetAddress)` — two overloads sharing a name. The test's lambda
implements only the abstract one. The cast `(InetSocketAddress) null`
statically selects the default-method overload at the call site, which
should run `Assert.notNull` and throw `IllegalArgumentException` before
ever reaching the lambda body — but under CratonVM, no exception is thrown
at all. The sibling tests in the same nested class (`whenIpv4`, `whenIpv6`,
`whenLambda`) all pass, isolating the defect to the null-argument path
specifically.

## Root cause (hypothesis, unconfirmed)

Not pinned to a file:line in CratonVM source. The shape (a default
interface method that should run first and throw, apparently bypassed in
favor of directly invoking the lambda's abstract-method implementation)
looks like a default-method-vs-abstract-method dispatch resolution issue —
possibly `invokeinterface`/default-method resolution matching by name
without fully honoring the descriptor, or a lambda-proxy dispatch shortcut
that routes straight to the functional interface's single abstract method
regardless of which overload was statically selected at the call site. No
concrete defect was located in the interpreter's invokeinterface/default-method
resolution path this session — this needs a dedicated bisection (e.g. a
minimal standalone repro with two same-named default/abstract interface
methods) rather than further guessing from this log alone.

**Documentation check:** no existing doc references `InetAddressFilter`,
`matches(InetSocketAddress)`, or this default/abstract-overload-collision
shape. Not confirmed to relate to any of the other `spring-boot-http-client`
clusters filed the same day (`jdk-httpclient-builder-config-loss-cluster.md`,
`tls-sslbundle-trust-validation-gap-cluster.md`,
`httpclient-autoconfigure-classpath-presence-cluster.md`) — this is a
low-level unit test of `InetAddressFilter` itself, not going through
`HttpClient.send()` or any builder at all.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.InetAddressFilterTests` (1 of 76 tests) |
