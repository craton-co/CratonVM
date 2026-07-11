# `io.smallrye.common.os.Process`/`java.lang.ProcessHandle` incompletely implemented — linkage error in one module, AbstractMethodError in another

Status: FIXED (2026-07-11) — the native-backed `ProcessHandle` and
`ProcessHandle.Info` surface is now verifier-visible and callable in real-JDK
mode.

Date observed: 2026-07-10/11 (fresh-binary rerun from updated dev, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Resolution

`ProcessHandle` and `ProcessHandle.Info` now have a deliberately narrow
native-backed fallback when their JDK class-file lookup misses. The fallback
declares both interface types and all supported methods, so third-party field
descriptors link correctly. Real-JDK VM initialization now registers the same
native `ProcessHandle` bridge used in synthetic mode, including `current()`,
`info()`, and the `Info` accessors. Optional-valued methods return genuine
empty `Optional` objects rather than Java `null`.

Verified on the Azure Linux host with the unique release binary
`/data/data/cratonvm-processhandle-smallrye-20260711`:

- A standalone class with `ProcessHandle`/`ProcessHandle.Info` static fields
  executed `current().info().command()` and printed `process-handle-link-ok=true`.
- The exact reported `smallrye-common-os-2.16.0.jar` loaded
  `io.smallrye.common.os.Process` through `Class.forName` and printed
  `smallrye-process-load-ok`.

## Summary

At least 37 `testsuite/model` classes CRASH with the identical signature, always at class-load time before any
test body runs:

```
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error: no class def found: io/smallrye/common/os/Process
```

Affected classes span most of `testsuite/model`'s `session`/`user`/`transaction`/`authz` packages —
`UserModelTest`, `UserPaginationTest`, `UserSyncTest`, `FederatedUserTest`, `UserSessionProviderModelTest`,
`UserSessionProviderOfflineModelTest`, `UserSessionPersisterProviderTest`, `SingleUseObjectModelTest`,
`StorageTransactionTest`, `TimeOffsetTest`, `ConcurrentAuthzTest`, and more.

## Root cause — confirmed by direct inspection of the class and CratonVM's own native registrations

`io.smallrye.common.os.Process` (from `smallrye-common-os-2.16.0.jar`, confirmed present on the classpath and
inside the jar via `jar tf`/`javap`) is a completely ordinary, real class:

```java
public final class io.smallrye.common.os.Process {
    private static final java.lang.ProcessHandle current;
    private static final java.lang.ProcessHandle$Info currentInfo;
    private static final java.lang.String name;
    ...
}
```

It has two `private static final` fields typed `java.lang.ProcessHandle` and `java.lang.ProcessHandle$Info`.

CratonVM's `java.lang.ProcessHandle` and `java.lang.ProcessHandle$Info` are **not real, bytecode-backed JDK
classes** in this VM — they only exist as purely synthetic, native-allocated objects
(`native-builtins/src/phases_late.rs`, via `alloc_concurrent_synthetic(ctx, "java/lang/ProcessHandle", 1)` /
`alloc_concurrent_synthetic(ctx, "java/lang/ProcessHandle$Info", 0)`), created on-demand only by specific native
method calls (`Process.toHandle()`, `ProcessHandle.current()`, etc.) that have their own registry entries. There
is no actual `.class` definition CratonVM can hand back when some *other*, ordinary class merely **references
`ProcessHandle`/`ProcessHandle$Info` as a field type** in its own class file — verification/linking of that
referencing class (`io.smallrye.common.os.Process` here) needs to resolve those type references, and since
there's no real class backing them, resolution fails. The linkage error is attributed to the class being
verified (`io/smallrye/common/os/Process`) rather than the actual missing piece (`java/lang/ProcessHandle`),
which is a bit misleading but consistent with this hypothesis — this specific class never even calls into
`ProcessHandle` at runtime in its static initializer failure path, it just *declares fields of that type*.

## Second manifestation — confirms this is a `ProcessHandle` gap, not something specific to linking

`tests/clustering :: org.keycloak.tests.clustering.JdbcPingCustomSchemaTest::testClusterFormed` fails via the
exact same `io.smallrye.common.os.Process.<clinit>` call site, but with a *different* symptom:

```
=> java.lang.AbstractMethodError: method java/lang/ProcessHandle.info()Ljava/lang/ProcessHandle$Info; has no Code attribute
   io.smallrye.common.os.Process.<clinit>(Process.java:34)
   org.jboss.logmanager.ExtLogRecord.<init>(ExtLogRecord.java:90)
   ...
   org.testcontainers.containers.PostgreSQLContainer.<init>(PostgreSQLContainer.java:60)
   org.keycloak.testframework.database.PostgresTestDatabase.createContainer(PostgresTestDatabase.java:20)
```

Here, `ProcessHandle` as a *type* resolves fine (no linkage error at class-load) — but the specific *method*
`ProcessHandle.info()` throws `AbstractMethodError: ... has no Code attribute` when actually invoked from
`Process.<clinit>` (matching the `currentInfo` field seen in the class structure above, i.e.
`currentInfo = ProcessHandle.current().info()`). This is a **second, independent way the same
incompletely-implemented `ProcessHandle` surfaces**: one path (a bare *field-type reference*, causing
verification/resolution to fail outright) produces "no class def found"; another path (an actual *method
invocation* on an instance of the synthetic class) produces "has no Code attribute" — both point at the same
underlying gap (CratonVM's `java/lang/ProcessHandle` is a `alloc_concurrent_synthetic`-only construct without a
complete, verifier-satisfying real class shape), just tripped by different code paths through the same
third-party dependency (`smallrye-common-os`).

This is also a third confirmed instance, alongside `X509Extension.getExtensionValue` (already documented in
`docs/known-issues/keycloak-07-04/`) and `TypeVariable.getAnnotatedBounds()` (see
`typevariable-getannotatedbounds-abstractmethoderror-mockito-bytebuddy.md` in this same folder), of CratonVM
having JDK reflection/interface methods that are declared in its class metadata but lack a real, callable Code
body — see that doc's "systemic gap class" note for the broader pattern worth auditing.

## Why this specific pattern is worth fixing generally

This isn't really about `io.smallrye.common.os.Process` in particular — it's that **any third-party class with a
field, parameter, or return type of `java.lang.ProcessHandle`/`ProcessHandle$Info` will fail to link under
CratonVM**, even if that class never actually calls a `ProcessHandle` method at runtime. Synthetic/native-only
classes need *some* minimal real class definition (even a trivial "shape" with fields matching what
`alloc_concurrent_synthetic` allocates) so ordinary classfile verification/resolution succeeds for third-party
code that merely mentions the type, independent of whether the native method registrations backing its actual
behavior are used or not.

## Original investigation notes

1. Search `native-builtins/src/` for how CratonVM registers classes as "real but native-backed" elsewhere (e.g.
   how `java.lang.Process` itself, or other similarly-synthetic JDK classes, are exposed to the classloader/
   verifier — there may be an existing pattern for "give this class a real shape but keep native method bodies"
   that `ProcessHandle`/`ProcessHandle$Info` should adopt instead of pure `alloc_concurrent_synthetic`).
2. Confirm the fix by re-running one of the 37 affected classes and checking that class loading succeeds (even
   if `io.smallrye.common.os.Process`'s actual static-init logic that calls `ProcessHandle.current()` still needs
   the native method registrations to behave sensibly afterward).
3. Given the blast radius (37+ classes in one 300-class shard alone, likely more across the full ~1147-class
   "others" set), this is comparable in scale to the already-fixed X.509 `AuthorityKeyIdentifier` bug from the
   2026-07-07 session — worth prioritizing.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-smallrye-process-linkage -ClassList <(printf 'module\tclass\ntestsuite/model\torg.keycloak.testsuite.model.user.UserModelTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-20260710.exe -JdkHome $jdk
```

Minimal standalone repro (no Keycloak needed): compile and run any class with a field
`private static final java.lang.ProcessHandle x = ProcessHandle.current();` under CratonVM — the class itself
should fail to link, independent of Keycloak/Infinispan.

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-v2-20260710-shard4\others-jit\logs\testsuite_model.org.keycloak.testsuite.model.user.{UserModelTest,UserPaginationTest,...}.err.log`
(37 total in this shard alone), 2026-07-10 rerun with a binary built from current `dev`. Class inspection via
`javap -p -classpath .../smallrye-common-os-2.16.0.jar io.smallrye.common.os.Process`. CratonVM source:
`native-builtins/src/phases_late.rs` (search `ProcessHandle`).
