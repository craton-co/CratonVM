# Bug B — Mockito dispatch corruption with combined `mockStatic` + `mock()`/`mockConstruction`

| | |
|---|---|
| **Severity** | High (Mockito is pervasive in the Kafka suite) |
| **Kind** | Wrong dispatch (mock returns the wrong stub / fails to intercept) |
| **Surfaced by** | `org.apache.kafka.clients.ClientUtilsTest` (`testParseAndValidateAddressesWithReverseLookup`) |
| **CratonVM** | FAIL · **HotSpot** OK |
| **Status** | OPEN — root-caused to the Mockito-on-CratonVM dispatch layer; **recommended for handoff** |
| **Recommendation** | Handoff (deep mock-maker / dispatch work; same family as the known Mockito bug-09 / bug-24 cluster) |

## Symptom

In `ClientUtilsTest.testParseAndValidateAddressesWithReverseLookup`, production code
(`ClientUtils.parseAndValidateAddresses`) calls `address.isUnresolved()` on a
construction-mocked `InetSocketAddress` and Mockito throws:

```
org.mockito.exceptions.misusing.WrongTypeOfReturnValue:
String cannot be returned by isUnresolved()
isUnresolved() should return boolean
```

i.e. the call to the `boolean isUnresolved()` mock returned the **String** value that was
stubbed for a *different* method (`getHostName()`). The mock dispatched one method's call
to another method's stub.

## What triggers it

The failing test combines three Mockito mechanisms on the **same / related classes**:

```java
try (MockedStatic<InetAddress> s = mockStatic(InetAddress.class)) {
    InetAddress a1 = mock(InetAddress.class);
    when(a1.getCanonicalHostName()).thenReturn("canonical1");   // instance mock
    ...
    try (MockedConstruction<InetSocketAddress> mc = mockConstruction(InetSocketAddress.class,
            (mock, ctx) -> {
                when(mock.isUnresolved()).thenReturn(false);
                when(mock.getHostName()).thenReturn((String) ctx.arguments().get(0));
                when(mock.getPort()).thenReturn((Integer) ctx.arguments().get(1));
            })) {
        ClientUtils.parseAndValidateAddresses(List.of("example.com:10000"), USE_ALL_DNS_IPS);
    }
}
```

- **`mockConstruction(InetSocketAddress)` alone works** on CratonVM (verified:
  `isUnresolved()/getHostName()/getPort()` all return their stubbed values).
- The failure needs the **combination**: an active `mockStatic` *plus* instance `mock()`
  of `InetAddress` *plus* `mockConstruction` of `InetSocketAddress`, with the production
  code driving the calls.

## Repro

`ksuite/repro/MockClientUtils.java` — standalone (no JUnit), drives the real
`ClientUtils.parseAndValidateAddresses` under the same mock setup.

- **HotSpot:** `OK validated=1 hostnames=[example.com]`.
- **CratonVM:** fails. In the standalone repro it fails one step earlier —
  `when(a1.getCanonicalHostName())` raises
  `MissingMethodInvocationException ("when() requires an argument which has to be 'a method
  call on a mock'")` — i.e. the instance-mock method call isn't registered as a mock
  invocation while `mockStatic(InetAddress)` is active. Inside the full JUnit test it gets
  one step further and misroutes `isUnresolved()` → the `getHostName()` String stub.

Both faces are the same defect: **when `mockStatic` is active for a class, method calls on
mocks of that (or a closely related) class are not dispatched to the correct Mockito
invocation handler** — they either aren't intercepted (→ `MissingMethodInvocationException`)
or resolve to the wrong stubbed method (→ `WrongTypeOfReturnValue`).

## Why handoff

This lives in CratonVM's Mockito/inline-mock-maker dispatch (ByteBuddy-generated subclass
method routing + `MockedStatic`/`MockedConstruction` scope interaction), the same area as
the previously tracked Mockito self-attach (bug-09) and JIT MIC/PIC mock crash (bug-24).
It is not a small localized bug like Bug A; it needs someone with context on the mock
dispatch layer.

## Related

A large `consumer.internals.*` **hang cluster** in this same run is also Mockito + timer/
background-thread heavy — see the hang-cluster doc; some of those may share dispatch-layer
root causes with this bug.
