# Keycloak RealmModelTest no-JIT H2 auth failure after Liquibase completes

Status: fixed/retired

Date observed: 2026-07-09

## Summary

After the Liquibase checksum/status/update timeout was fixed, `RealmModelTest`
under CratonVM `--nojit` now runs all 195 Keycloak Liquibase changesets and
logs database-update completion. The class then fails during a later
`KeycloakModelTest` static initialization pass when Hibernate tries to create
`JdbcEnvironment` and H2 rejects the connection credentials:

```text
org.hibernate.service.spi.ServiceException:
Unable to create requested service [org.hibernate.engine.jdbc.env.spi.JdbcEnvironment]
due to: Error calling Driver.connect() [Wrong user name or password [28000-240]]
```

This is no longer a Liquibase timeout. It is a later H2/Hibernate credential
propagation residual in the same `RealmModelTest` class.

## Fixed evidence

Retired on 2026-07-10 after the focused Keycloak model class passed on the Azure host from the isolated worktree branch `codex/fix-keycloak-realmmodel-h2-auth-20260709-123713`.

Validation binary:

```text
/data/data/cargo-targets/keycloak-realmmodel-h2-auth-20260709-123713-baseline/release/cratonvm-keycloak-realmmodel-h2-auth-20260709-123713-fixed
```

Runner result:

```text
run: verify-realmmodel-h2-auth-fixed-jdk25-20260709-123713-r109-final-candidate-700
mode: all-nojit
status: PASS
seconds: 352.304
tests: 3
failed: 0
```

The final run no longer reproduced the tracked `Wrong user name or password [28000-240]` H2 bootstrap failure, the post-Liquibase timeout, the intermediate H2 `Command` cast failure, or the `java.util.Map.forEach` localization NPE. The relevant runtime fixes are the H2 `SessionLocal.prepareLocal` no-cache bridge plus the receiver-aware `Map.forEach` path for Hibernate `PersistentMap` backed by arbitrary map implementations.

## Evidence

Validation binary:

```text
C:\craton\cargo-targets\keycloak-liquibase-columnconfig-20260709-012\release\cratonvm-keycloak-liquibase-columnconfig-20260709-012.exe
```

Runner result:

```text
run: keycloak-liquibase-columnconfig-verify-20260709-012
mode: others-nojit
status: FAIL
seconds: 881.085
note: => java.lang.ExceptionInInitializerError
```

Liquibase completion evidence from stderr:

```text
ChangeSet META-INF/jpa-changelog-26.7.0.xml::26.7.0-outbox::keycloak ran successfully
Run:                        195
Total change sets:          195
Completed database update for changelog {0}
```

Terminal stack from stdout:

```text
JUnit Vintage:RealmModelTest
=> java.lang.ExceptionInInitializerError
org.keycloak.testsuite.model.KeycloakModelTest.reinitializeKeycloakSessionFactory(KeycloakModelTest.java:374)
org.keycloak.testsuite.model.KeycloakModelTest.<clinit>(KeycloakModelTest.java:306)
Caused by: org.hibernate.exception.AuthException:
Error calling Driver.connect() [Wrong user name or password [28000-240]]
Caused by: org.h2.jdbc.JdbcSQLInvalidAuthorizationSpecException:
Wrong user name or password [28000-240]
```

There is older internal Hibernate-suite history for this signature in
`docs/internal/hibernate-bugs/HIB-CV-06-emf-bootstrap-db-connect.md`, where
the known root cause was `Properties` credential pollution. Treat this
Keycloak failure as a fresh residual until a focused probe proves it is the
same mechanism or a new credential propagation bug.

## Next leads

- Instrument H2 `Driver.connect` or Hibernate
  `DriverConnectionCreator.makeConnection` to log the URL, user, and password
  shape for the first successful Liquibase connection and the later failing
  Hibernate bootstrap connection in the same VM.
- Compare with HotSpot `-Xint` using the same Keycloak runner class list.
- Check `Properties` and `Map` credential propagation on the Keycloak
  `DefaultJpaConnectionProviderFactory` / Hibernate settings path, using
  `HIB-CV-06` as prior art but not assuming it is the same bug.
