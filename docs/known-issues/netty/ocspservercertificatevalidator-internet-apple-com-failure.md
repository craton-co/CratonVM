# `OcspServerCertificateValidatorTest` fails on external network call to `apple.com:443`

| | |
|---|---|
| **Status** | External network dependency / harness isolation gap. NOT a CratonVM defect. |
| **Scope** | 1 class: `io.netty.handler.ssl.ocsp.OcspServerCertificateValidatorTest`. |
| **Discovered** | 2026-09-23, full 739-class suite run (`run-20260923-192409-passed`). |

## Symptom

`OcspServerCertificateValidatorTest.connectUsingHttpAndValidateCertificateUsingOcspTest()` fails on assertion:

```
org.opentest4j.AssertionFailedError: expected: <true> but was: <false>
    at org.junit.jupiter.api.AssertionFailureBuilder.build(AssertionFailureBuilder.java:151)
    at org.junit.jupiter.api.AssertionFailureBuilder.buildAndThrow(AssertionFailureBuilder.java:132)
    at org.junit.jupiter.api.AssertTrue.failNotTrue(AssertTrue.java:63)
    at org.junit.jupiter.api.AssertTrue.assertTrue(AssertTrue.java:36)
    at org.junit.jupiter.api.Assertions.assertTrue(Assertions.java:183)
    at io.netty.handler.ssl.ocsp.OcspServerCertificateValidatorTest.connectUsingHttpAndValidateCertificateUsingOcspTest(OcspServerCertificateValidatorTest.java:89)
```

## Root Cause Analysis

Source inspection of `OcspServerCertificateValidatorTest.java:84-89`:

```java
ChannelFuture channelFuture = bootstrap.connect("apple.com", 443);
channelFuture.sync();

// Wait for maximum of 1 minute for Ocsp validation to happen
latch.await(1, TimeUnit.MINUTES);
assertTrue(ocspStatus.get());
```

The test connects directly over the public internet to `apple.com:443`, queries Apple's public OCSP responder via HTTP, and asserts that an `OcspValidationEvent` was received with `event.response().status() == OcspResponse.Status.VALID`.

In isolated CI environments, air-gapped test runners, or environments with firewall rules or DNS restrictions blocking public outbound traffic to Apple's OCSP responder, the latch times out after 1 minute without receiving a valid OCSP response, leaving `ocspStatus` as `false`.

## Disposition

Harness / environmental external dependency. Unit/integration tests should not depend on live public internet services. In environments without open outbound access to Apple's OCSP responder, this test should be excluded or marked with an assumption.
