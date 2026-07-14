# Keycloak model bootstrap: `KeycloakSession.realms()` returns null after JMX initialization

Status: open. Observed 2026-07-14 while verifying the retired
`management-notificationemittersupport-listenerlock-null` issue.

## Repro

Run `org.keycloak.testsuite.model.authz.ConcurrentAuthzTest` under the
real-JDK CratonVM Keycloak model harness after the JMX notification-emitter
repair. The test now passes Infinispan/Micrometer initialization and proceeds
to `DefaultKeycloakSessionFactory.publish`.

## Failure

```text
java.lang.NullPointerException: Cannot invoke
"org.keycloak.models.RealmProvider.getRealmsWithProviderTypeStream(java.lang.Class)"
because the return value of "org.keycloak.models.KeycloakSession.realms()" is null
    at org.keycloak.storage.UserStorageEventListener.lambda$onEvent$3
    at org.keycloak.services.DefaultKeycloakSessionFactory.publish
```

## Scope

This is not a residual of `NotificationEmitterSupport`: the former
`listenerLock` failure no longer appears in the same full model bootstrap.
Trace the `KeycloakSession` provider lookup and the `RealmProvider` registration
path during the session-factory publish event.