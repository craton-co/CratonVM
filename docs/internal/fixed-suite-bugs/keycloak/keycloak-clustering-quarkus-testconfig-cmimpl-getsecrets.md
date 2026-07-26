# Keycloak Quarkus config-mapping implementation stubs missing getSecrets

Status: FIXED (2026-07-03, branch `fix/keycloak-testconfig-cmimpl-getsecrets-20260703`)

Date observed: 2026-07-03

## Summary

Keycloak classes crashed or failed under CratonVM during JUnit launcher /
test-framework startup with method-resolution failures on Quarkus'
runtime-generated SmallRye config-mapping implementations.

The first observed bucket was both `tests/clustering` classes, failing on
`TestConfig$$CMImpl`:

```text
NoSuchMethodError
method="io/quarkus/deployment/dev/testing/TestConfig$$CMImpl.getSecrets()Ljava/util/Set;
[class not found on any classpath entry - synthetic stub, add the missing jar]"
caller="io/smallrye/config/ConfigMappingLoader.configMappingSecrets(Ljava/lang/Class;)Ljava/util/Set; @pc=16"
```

Java-visible exception:

```text
java/lang/reflect/UndeclaredThrowableException
Caused by: java/lang/NoSuchMethodError:
io/quarkus/deployment/dev/testing/TestConfig$$CMImpl.getSecrets()Ljava/util/Set;
```

Affected classes:

```text
org.keycloak.tests.clustering.JdbcPingCustomSchemaTest
org.keycloak.tests.compatibility.ClusteredOAuthClientTest
```

The later non-passed rerun also exposed the same root cause as 345 `FAIL` rows
in the Keycloak JUnit 5 test-framework modules, failing on
`LogBuildTimeConfig$$CMImpl` during `LogHandler.initializeQuarkusLogging`:

```text
NoSuchMethodError
method="io/quarkus/runtime/logging/LogBuildTimeConfig$$CMImpl.getSecrets()Ljava/util/Set;
[class not found on any classpath entry - synthetic stub, add the missing jar]"
caller="io/smallrye/config/ConfigMappingLoader.configMappingSecrets(Ljava/lang/Class;)Ljava/util/Set; @pc=16"
```

Affected rows in `craton-nonpassed-dev-20260703-01 / others-jit`:

```text
tests/base       341
tests/webauthn     4
total            345
```

Representative rerun logs:

```text
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\logs\tests_base.org.keycloak.tests.account.AccountConsoleDisabledTest.err.log
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\logs\tests_webauthn.org.keycloak.tests.webauthn.WebAuthnRegisterAndLoginTest.err.log
```

## Root cause (confirmed via bytecode disassembly of the real
`smallrye-config-core-3.16.0.jar`)

Classes such as `io.quarkus.deployment.dev.testing.TestConfig$$CMImpl` and
`io.quarkus.runtime.logging.LogBuildTimeConfig$$CMImpl` are **not** classes
that ship in any jar. They are generated **at runtime** by SmallRye Config's
`ConfigMappingLoader`, via a well-defined, deliberate two-step protocol:

1. `ConfigMappingLoader.loadImplementation`/`loadClass` first call
   `classLoader.loadClass("io.quarkus.deployment.dev.testing.TestConfig$$CMImpl")`,
   **expecting `ClassNotFoundException`** (verified: both methods wrap this
   exact call in an exception table entry catching `ClassNotFoundException`
   and, on catch, fall through to the generation step).
2. On `ClassNotFoundException` (or an assignability mismatch), SmallRye
   ASM-generates the real implementation bytecode and defines it via
   `io.smallrye.common.classloader.ClassDefiner.defineClass(Lookup, Class,
   String, byte[])`, which CratonVM handles through the standard
   `MethodHandles.Lookup.defineClass(byte[])` native.

CratonVM's classloader never threw `ClassNotFoundException` at step 1.
`io/quarkus/` is one of the `is_enterprise_stub_prefix` namespaces that get a
fabricated synthetic stub when absent from the classpath (deliberate,
load-bearing behavior for WildFly/Quarkus bytecode linkage elsewhere). The
`reflective_probe` gate that correctly forces a real `ClassNotFoundException`
for `Class.forName`-style existence probes (see BUG-06) only covers
`Class.forName`, not plain `ClassLoader.loadClass(String)` — which is exactly
what SmallRye calls here. So CratonVM silently fabricated an empty stub named
`TestConfig$$CMImpl` under `ClassLoaderId::Bootstrap` (see
`create_synthetic_stub`).

SmallRye's own generation step *did* still run afterward (bytecode
generation isn't gated on the `ClassNotFoundException` alone — the
`isAssignableFrom` check on the stub also fails, per the disassembled
bytecode) and correctly defined the REAL class via
`MethodHandles.Lookup.defineClass(byte[])`. But
`ClassManager::define_class_with_options` minted a **brand-new, second
`ClassId`** for that real bytecode under the caller's loader (Application),
distinct from the earlier Bootstrap-registered stub — it never checked
whether a stub of the same name already existed elsewhere.

The stub and the real class then coexisted under the identical name in two
different loader slots. Every subsequent by-name resolution
(`get_loaded_class_id`, used by `Class.forName`, `ClassLoader.loadClass`,
`MethodHandles.Lookup.findStatic`/`findConstructor`, and constant-pool
resolution) walks the built-in loader delegation chain
**Bootstrap → Extension → Application** and returns the **first** match — so
it permanently resolved to the empty Bootstrap stub, never the real,
just-defined class. `ConfigMappingLoader$ConfigMappingImplementation`'s
constructor calls `Lookup.findStatic(implementationClass, "getSecrets", ...)`
against that resolved (stub) class, which has no methods, and the resulting
real `java.lang.NoSuchMethodException` is converted by SmallRye's own code
into the observed `NoSuchMethodError`.

## Fix

`../../../../classloading/src/class_manager.rs` — `ClassManager::define_class_with_options`:
before minting a new `ClassId`, check whether a class of the exact same name
already exists (`get_loaded_class_id`) and is a synthetic stub. If so, upgrade
that existing stub **in place** via the pre-existing `upgrade_synthetic_class`
machinery (the same self-healing path `load_class` already uses when a
stub's real `.class` file later appears on the classpath), instead of minting
a second, permanently-shadowed registration. This makes the real bytecode
visible to every subsequent by-name lookup, matching what SmallRye's runtime
`@ConfigMapping` generation protocol (and any other code relying on
"probe via `loadClass`, generate+define on miss") requires.

Regression test:
`classloading/tests/wp2_3_define_class_backend.rs::defining_real_bytecode_upgrades_existing_enterprise_stub_in_place`
— fabricates an enterprise-stub-eligible name via `load_class` on an empty
classpath (mirrors the failed `ClassLoader.loadClass` probe), then defines
real bytecode with a genuine `getSecrets` method under that same name, and
asserts the ClassId is upgraded in place (not shadowed) and the real method
is reachable.

## Verification

Re-ran the exact repro from this doc
(`org.keycloak.tests.clustering.JdbcPingCustomSchemaTest`, `tests/clustering`
module classpath, JIT on) against the fixed binary. The
`TestConfig$$CMImpl.getSecrets()` `NoSuchMethodError` is gone — the JUnit
launcher session now progresses well past
`ConfigMappings$ConfigClass.configClass`/`ConfigMappingLoader.configMappingSecrets`
into `LauncherFactory.collectLauncherInterceptors`, where it hits a
**different, unrelated** `NullPointerException`
(`Cannot invoke "java.lang.Boolean.booleanValue()" because the return value
of "java.util.Optional.orElse(Object)" is null`, at `KcRunner.main`
→ `SessionPerRequestLauncher.execute` → `...collectLauncherInterceptors`).
That was a separate frontier, not fixed by this change.

**Update (2026-07-03, same day):** a concurrent session independently found
and fixed this exact residual — commit `80e471fc` "fix(reflection): remove
native `<clinit>` override that broke Boolean/Integer/etc. static field init
in real-JDK mode". Root cause: a native `<clinit>` override for the primitive
wrapper types (`register_primitive_wrapper_type_clinits`) replaced their real
bytecode entirely and hardcoded slot 0 as `TYPE`, but real JDK 25's
`java.lang.Boolean` has slot 0 = `TRUE` — so `Boolean.FALSE` was left `null`,
and `Optional<Boolean>.orElse(false)` (used by
`org.junit.platform.launcher.core.LauncherFactory`) returned that null,
producing the exact `NullPointerException` seen here. Both fixes are now
merged into `dev`; the two Keycloak clustering classes should be re-run
end-to-end to confirm they now pass (or surface a further frontier) — not
done as part of this doc.
