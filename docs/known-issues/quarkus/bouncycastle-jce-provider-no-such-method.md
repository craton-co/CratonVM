# Quarkus: BouncyCastle / JCE Provider `NoSuchMethodError` in Security and gRPC TLS

## Status
**OPEN, root-caused.** Discovered during Quarkus test suite failures triage (`run-20260914-052033-passed`).

## Symptom
gRPC TLS, Keycloak, and OIDC Security integration tests throw `NoSuchMethodError` when initializing BouncyCastle (`org.bouncycastle.jce.provider.BouncyCastleProvider`) or security provider registries:

```
java.lang.NoSuchMethodError: 'void org.bouncycastle.jcajce.provider.config.ProviderConfiguration.addAlgorithmSecurityProvider(java.lang.String, java.lang.String)'
	at org.bouncycastle.jce.provider.BouncyCastleProvider.setup(BouncyCastleProvider.java:312)
```

## Root Cause
When BouncyCastle initializes its security provider dynamically, CratonVM's security provider registry (`native-builtins-security`) or classloader isolation intercepts or redirects provider class loading.

Because CratonVM's security builtins shadow certain `java.security.Provider` and JCE provider classes, BouncyCastle classes loaded across isolated classloader boundaries resolve against CratonVM's synthetic stub definitions rather than the full BouncyCastle JAR bytecode on the application classpath.

## Affected Tests / Scenarios
- Quarkus gRPC TLS tests (`GrpcTlsTest`, `KeycloakServerTest`)
- OIDC Security Provider initialization tests

## Remediation / Solution Plan
1. Audit `native-builtins-security` class origin and resolution order for `org.bouncycastle.*` and `java.security.Provider`.
2. Ensure real bytecode from application classpath takes precedence over compatibility stubs per `AGENTS.md` load-bearing rules ("Real class bytes are authoritative over registered natives").
