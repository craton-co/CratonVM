# Keycloak model bootstrap: `KeycloakSession.realms()` null was a probe-classpath false positive

Status: resolved 2026-07-14.

## Finding

The reported `KeycloakSession.realms()` null was not a CratonVM provider
registration failure. The first follow-up probe was run without the documented
`-Dkeycloak.model.parameters=Infinispan,Jpa` setting and without Keycloak's
`keycloak-model-jpa-26.6.1.jar`. Consequently no
`RealmProviderFactory` descriptor was visible, so the test harness correctly
had no `RealmProvider` to return.

## Verification

The corrected real-JDK `ConcurrentAuthzTest` command used both the model
parameters and the JPA provider JAR. It passed the former
`UserStorageEventListener` / `KeycloakSession.realms()` path; neither the
provider-null NPE nor the preceding JMX listener-lock NPE occurred.

The model run then reached a later independent
`org.infinispan.protostream.DescriptorParserException`, which is outside this
false-positive report's scope.