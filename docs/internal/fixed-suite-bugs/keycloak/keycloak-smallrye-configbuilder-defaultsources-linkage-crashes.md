# Keycloak SmallRyeConfigBuilder addDefaultSources linkage crashes

Status: fixed (both classpath gaps closed; a separate, pre-existing,
already-tracked quarkus-core gap is what remains — see Follow-on issue).

Date observed: 2026-07-02
Date fixed: 2026-07-02

## Summary

The expanded Keycloak `tests` and `testsuite` CratonVM run recorded 3 `CRASH`
rows with this linkage warning:

```text
NoSuchMethodError
method="io/smallrye/config/SmallRyeConfigBuilder.addDefaultSources()Lio/smallrye/config/SmallRyeConfigBuilder;"
caller="org/keycloak/testframework/config/Config.initConfig()Lio/smallrye/config/SmallRyeConfig; @pc=10"
```

Affected classes:

```text
tests/base org.keycloak.tests.db.CaseSensitiveSchemaTest
tests/base org.keycloak.tests.db.PreserveSchemaCaseLiquibaseTest
tests/clustering org.keycloak.tests.clustering.JdbcPingCustomSchemaTest
```

## Root cause (two layered classpath gaps, same shape as the assertNotNull bug)

`smallrye-config` / `smallrye-config-common` / `smallrye-config-core` were
entirely absent from `apps\keycloak\kc-universal-cp.txt` — `io/smallrye/`
is one of the "enterprise stub prefix" packages CratonVM's classloader
silently fabricates an empty stub class for instead of raising
`ClassNotFoundException` (see the sibling doc,
[`keycloak-smallrye-assertnotnull-linkage-crashes.md`](keycloak-smallrye-assertnotnull-linkage-crashes.md),
for the full mechanism), so the missing `SmallRyeConfigBuilder` resolved to
a methodless stub and `addDefaultSources()` on it produced `NoSuchMethodError`.

Adding those three jars (version 3.16.0, matching the version already used
by `apps\keycloak\quarkus\config-api\cratonvm-full-cp.txt`) fixed the
`addDefaultSources` `NoSuchMethodError` but immediately exposed a **second**
layer of the same problem one level down: the *real*
`SmallRyeConfigBuilder` class (now genuinely on the classpath) declares
`implements org.eclipse.microprofile.config.spi.ConfigBuilder`, and
`microprofile-config-api` was **also** entirely absent from the classpath.
Because the interface itself couldn't resolve, loading the real
`SmallRyeConfigBuilder.class` failed outright — this is correct JVMS
behavior for a class whose declared superinterface can't be found, not the
enterprise-stub mechanism — and surfaced as:

```text
java.lang.NoClassDefFoundError: io/smallrye/config/SmallRyeConfigBuilder
  org.keycloak.testframework.config.Config.initConfig(Config.java:87)
```

## Fix

`apps\keycloak\kc-universal-cp.txt` is a local, gitignored, machine-generated
file (not checked in, no in-repo generator script), so the fix was applied
directly to that file on this machine. In addition to the
`smallrye-config`/`smallrye-config-common`/`smallrye-config-core` 3.16.0 jars
(added first, closing the `addDefaultSources` `NoSuchMethodError`),
`microprofile-config-api-3.1.jar` was appended (matching the version already
used in `apps\keycloak\quarkus\config-api\cratonvm-full-cp.txt`), from the
local Maven repo
(`~/.m2/repository/org/eclipse/microprofile/config/microprofile-config-api/3.1/`).

Verified with a direct repro (`KcRunner` against all three originally
affected classes on a freshly built `cratonvm.exe`), independently
re-confirmed by an adversarial re-verification pass that reran the exact
repro from scratch: neither the `addDefaultSources` `NoSuchMethodError` nor
the `SmallRyeConfigBuilder` `NoClassDefFoundError` occurs anymore. All three
classes now get far enough to reach JUnit's own failure reporting (a
`KCRUNNER_RESULT` line is printed and a normal JUnit `Failures (1):` summary
is emitted), instead of a hard top-level "linkage error ... process
terminating" abort.

### Diagnostics added (so this class of bug self-diagnoses next time)

Shared with the sibling `assertNotNull` fix — see that doc's "Diagnostics
added" section. Same two files
(`../../../../classloading/src/class_manager.rs`, `../../../../vm/src/vm/vm_exec.rs`), reviewed and
confirmed lock-safe / panic-safe / behavior-preserving by an independent
adversarial pass.

## Follow-on issue (separate bug, uncovered by this fix — NOT closed by it)

All three classes still fail, but no longer with a linkage crash — with a
normal, JUnit-caught exception one layer further into config bootstrap:

```text
java.lang.ExceptionInInitializerError
  org.keycloak.testframework.injection.Extensions.<init>(Extensions.java:43)
Caused by: java.lang.IllegalStateException: SRCFG00012: Can not add converter
  io.quarkus.runtime.configuration.CharsetConverter@... that is not parameterized with a type
  io.smallrye.config.SmallRyeConfigBuilder.withConverters(SmallRyeConfigBuilder.java:594)
  org.keycloak.testframework.config.Config.initConfig(Config.java:90)
```

This is the **same, already-tracked** `quarkus-core` classpath gap in
[`keycloak-testframework-quarkus-config-classpath-gap.md`](keycloak-testframework-quarkus-config-classpath-gap.md)
— that doc names `io/quarkus/runtime/configuration/CharsetConverter`
explicitly as one of the `quarkus-core` classes `Config.initConfig()` needs
and that are entirely absent from `kc-universal-cp.txt`. Because
`CharsetConverter` is absent, it too gets an enterprise-stub fallback (empty
class, no generic signature/interfaces), and real SmallRye code's own
`Converter<T>` generic-type introspection legitimately fails against that
signature-less stub with `SRCFG00012`. Not a CratonVM reflection/generics
bug — a real, correct SmallRye runtime check failing against an
intentionally-empty stand-in for a class that was never on the classpath.
No new doc needed; covered by the existing quarkus-core doc's next steps.
