# Quarkus suite: classes that fail on CratonVM only because the test's clock is shorter than our speed — 2026-10-24

The goal of the Quarkus waves is HotSpot-identical status; performance is out of scope. These classes
pass on HotSpot 25 and fail on CratonVM for no reason other than a wall-clock budget written into the
test (an Awaitility `atMost(5s)`, a JUnit `@Timeout(10s)`, a token lifespan of one second, a rate-limit
window of a few hundred milliseconds). Each one was diagnosed from its failure text and, where noted,
shown by a probe; none is a wrong answer. They stay here until the cold-path throughput gap closes.

Binary: `cvm-qw90.exe` (branch `claude/quarkus-known-issues-1cc4f7`); HotSpot oracle jdk-25.0.3.9. The virtual-thread pinning, AWT and Gradle dev-mode items once listed here are fixed (see `docs/internal/fixed-suite-bugs/quarkus/`).

## The RestAssured / Groovy first call

`io.restassured.RestAssured.given().get(url).body().jsonPath()` against a local `HttpServer`
(probe `C:\craton\rg\RT.java`):

| call | HotSpot | CratonVM |
|---|---|---|
| first | 1 464 ms | 27 618 ms |
| second, third | 13-16 ms | 900-920 ms |

Every Quarkus test that polls the application through RestAssured inside a short Awaitility window fails
on the first poll: the condition lambda itself runs longer than the window (`ConditionTimeoutException ...
Caused by: TimeoutException at FutureTask.get`, i.e. the lambda did not finish, not that its answer was
wrong).

* OpenTelemetry (5 s / 3 s / 30 s windows): `OpenTelemetryWithSpanAtStartupTest`, `OpenTelemetryTestCase`
  (reactive-messaging), `OpenTelemetryInjectionsTest`, `StaticResourceTest`, `OpenTelemetryQuartzTest`,
  `VertxOpenTelemetryTest`, `JvmMetricsTest`, `MpJvmMetricsTest`.
* `smallrye-graphql` `RequestLeakDetectionTest`, `PerRequestOrderingTest`, `resteasy-reactive`
  `RequestLeakDetectionTest` (10 s windows), `MultiByteFileTest` / `MultiByteWithRemoteErrorTest`
  (`@Timeout(10s)`).
* The six `*.security.inheritance.*ImplMethodSecuredTest` classes (about 40 s per test method).

## Token lifespans of one second

The Keycloak dev-service realm and the WireMock OIDC fixtures issue tokens whose `exp - iat` is 1 s.
`WARN OidcProvider: Verification of the token ... failed: The JWT is no longer valid` precedes every
failure; the page that should have been served is the login form, or the client assertion carries a
`refresh_token` grant where the stub expects `client_credentials`.

* `oidc-code-flow` `CodeFlowTest` (4 methods: `testBasicAuthAndCodeFlow`, `testCodeFlowNonce`,
  `testCodeFlowNoConsent`, `testCodeFlowForceHttpsRedirectUriWithQueryAndPkce`; stable across reruns, and
  `testCodeFlowNoConsent` alone fails with `401` on the callback with `exp` equal to the evaluation second).
* `OidcClientTest` (wiremock, 3 methods), `OidcClientRegistrationTest`, `GraphQLAuthExpiryDeferredTest`
  (4 parameterised cases), `KeycloakAdminClientZeroConfigDevServicesTest`.

## Other wall-clock assertions

* `awt` `ImageEncodersTest`: the encoders themselves are correct (no server error on qw91 after the
  UshortGray and ImagingLib/LCMS fixes); the test fails on the 3 s Awaitility window in `TestUtil` (about 1 s
  per request on HotSpot, much more here).
  HotSpot reference, same harness (2026-10-25): `ImageEncodersTest` PASS in 23 s (CratonVM 1 257 s),
  `ConnectorChangeTest` 10/10 in 99 s (CratonVM 0/10 in 1 919 s), `GrpcDevModeTest` 7/7 in 99 s (CratonVM 6/7 in 2 143 s).
* `RateLimitConfigTest.window()` (a fault-tolerance rate-limit window of a few hundred ms).
* `HyperLogLogCommandsTest`, `SetCommandsTest`, `HashCommandsTest` (redis datasource):
  `io.smallrye.mutiny.TimeoutException` on the first command of a method.
* `Http3DisabledTest`: the QUIC connect that must fail does not fail inside its timeout.
* `websockets-next` telemetry classes (`MetricsDisabledWebSocketsTest`, `MicrometerWebSockets{OnBinaryMessage,
  OnError,OnText,Standalone}Test`, `WebSocketsMetricsForPathWithParamTest`) and `WebsocketDevModeTestCase`:
  5 s Awaitility windows / `TimeoutException` (HotSpot passes each in 11-19 s total).
* `tls.PeriodicReloadTest`, `CertificateUpdatedEventWithPeriodicReloadTest` (one-second reload periods),
  `AeshSshIdleActivityResetTest` (idle-timeout window), `rest-client-reactive` `RequestLeakDetectionTest`
  (10 s windows).
* Dev-mode live reload takes 60-148 s on CratonVM against 1-18 s on HotSpot while the test windows are about 10 s (`ConnectorChangeTest`, the grpc server `GrpcDevModeTest` cases); `DevMcpTest` passes 12/12 with a 3000 s cap. Slower than the harness cap, not traced further (HotSpot 20-110 s): `DevUICorsTest`,
  `ConcurrentAuthTest`, `KafkaDevServicesContinuousTesting*TestCase`, `ConnectorChangeTest`,
  `GrpcDevModeTest`, `RedisClientDevModeTestCase`, `FluentApiMTLSAuthenticationRequiredDevModeTest`.
  Each is a dev-mode restart or a container start behind a RestAssured poll; the dev-mode restart itself
  works since the ANTLR intrinsic fix (`docs/internal/fixed-suite-bugs/quarkus/antlr-intrinsics-read-fields-by-class-name-dev-mode-restart-FIXED-20261024.md`).
* Micrometer (3 classes), Keycloak policy-enforcer (2 classes): wave 18 triage.
* `DevServicesKubernetesTest`: pulls an image.

## What would close it

Cold-start throughput of interpreted Groovy and of the first request path (the RestAssured probe above is
the reproducer). Nothing in these classes is expected to change once the clock is met.
