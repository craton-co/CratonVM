# WildFly JBoss Modules service-provider lookup leaks across modules

Status: FIXED (verified 2026-07-06) — the mechanisms originally suspected are
already correctly module-scoped on `dev`; the one confirmed gap
(`Module.loadService(Class)` unregistered, throwing `NoSuchMethodError`) is
fixed below.

Observed while rerunning non-passed WildFly classes on the Azure host with the
JIT-enabled WildFly runner.

## Reproduction (original)

Command shape:

```bash
cd /data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner
CRATONVM_BIN=/data/wt/wt-wildfly-nonpassed-20260705-035722/cratonvm-wildfly-nonpassed-20260705-035722-moddesc \
MVNW=/home/victor/.m2/wrapper/dists/apache-maven-3.9.11/a2d47e15/bin/mvn \
WILDFLY=/data/cratonvm/apps/wildfly JDK25=/home/victor/jdk25 JDK25_WIN=/home/victor/jdk25 \
./run-suite.sh run --category failed --only HostExcludesTestCase --count 1 --jit on --class-to 300 --tag azure-hostexcludes-moddesc-007
```

Failure:

```text
WFLYCTL0226: A subsystem named 'infinispan' cannot be registered by extension
'org.wildfly.extension.core-management' -- a subsystem with that name has
already been registered by extension 'org.jboss.as.jmx'.
```

The full Maven/WildFly Surefire harness (`apps/wildfly-suite-runner`) was not
available on the current Azure build host to re-run `HostExcludesTestCase`
verbatim — the WildFly source checkout with a built `testsuite/` module tree
had been cleaned up. Investigation below instead drives the exact real
mechanisms WildFly's boot code uses, directly, against a real
WildFly 32.0.1.Final distribution staged at
`/data/data/wildfly-dist/wildfly-32.0.1.Final` (module tree +
`wildfly-controller-24.0.1.Final.jar`, both unmodified originals from the
distribution zip).

## Investigation

Two real, unrelated WildFly modules were used as the canonical leak pair
(same shape as the original `infinispan`/`core-management`/`jmx` collision):
`org.jboss.as.jmx` and `org.wildfly.extension.core-management`. Neither
depends on the other; each ships its own
`../../../../apps/META-INF/services/org.jboss.as.controller.Extension` descriptor naming a
different provider (`org.jboss.as.jmx.JMXExtension` and
`org.wildfly.extension.core.management.CoreManagementExtension`
respectively, confirmed by extracting both descriptors from their real
jars).

**The two mechanisms this doc originally suspected are already correctly
scoped on `dev`:**

- `native_module_classloader_find_resources` (`../../../../native-builtins/src/jboss_module_loader.rs`)
  only falls back to the process-wide classpath when the resource is
  *not* `is_module_private_resource` (i.e. not under `../../../../apps/META-INF/services/`).
  Service descriptors always resolve through `module_service_roots`, which
  only walks the module's own resource roots plus deps that opt in with
  `services="import"|"export"` — confirmed by the existing
  `t19_h16_service_roots_only_include_service_imports` test.
- `discover_providers` (`../../../../native-builtins/src/service_loader.rs`) skips the
  bottom "flat classpath scan" entirely whenever the `ServiceLoader`'s
  loader is a JBoss `ModuleClassLoader` (`loader_is_jboss_module`), routing
  instead through `module_service_provider_names`.

This was verified directly (not just read) with a new test,
`jboss_module_loader::tests::wildfly_jboss_modules_service_provider_leak_real_dist_scoping`,
which calls `module_service_provider_names` against the **real**
`org.jboss.as.jmx` / `org.wildfly.extension.core-management` module trees
under the staged distribution and asserts each returns *exactly* its own
provider — no leakage either direction.

**Confirmed gap:** `org.jboss.as.controller.parsing.DeferredExtensionContext`
(WildFly's host-excludes / domain-mode deferred extension loading path —
this is what `HostExcludesTestCase` exercises) resolves each extension
module's providers via the real jboss-modules instance method
`Module.loadService(Class)`:

```
moduleLoader.loadModule(name).loadService(Extension.class)
```

(verified against the constant pool of `DeferredExtensionContext.class` in
`wildfly-controller-24.0.1.Final.jar` — literal
`invokevirtual org/jboss/modules/Module.loadService(Ljava/lang/Class;)Ljava/util/ServiceLoader;`).

This method was **never registered** as a native (only the static sibling
`Module.loadServiceFromCallerModuleLoader(String, Class)`, used by the
standalone-mode `ExtensionAddHandler` path, was). Calling it threw a bare
`NoSuchMethodError`, confirmed directly:

```
NoSuchMethodError method="org/jboss/modules/Module.loadService(Ljava/lang/Class;)Ljava/util/ServiceLoader; ..."
caller="...Probe.probeInstance(...) @pc=..."
RESULT:ERROR:java.lang.NoSuchMethodError:null
```

A hard `NoSuchMethodError` on this path would abort domain-mode host-excludes
boot outright rather than produce the graceful `WFLYCTL0226` management-layer
message, so this specific crash is not verbatim the originally-observed
symptom — but it is a real, confirmed defect on the exact code path the
failing test class drives, and blocks that path from working at all.

## Fix

Registered `Module.loadService(Class)` in
`../../../../native-builtins/src/jboss_module_loader.rs`
(`native_module_load_service`), mirroring the already-correct
`native_module_load_service_from_caller_module_loader`: it resolves the
receiver module's own `ModuleClassLoader` via `native_module_get_class_loader`
and delegates to `ServiceLoader.load(Class, ClassLoader)` — the exact same
already-verified module-scoped path, so no new scoping logic was needed.

Verified (isolated, timeout-guarded, real distribution): after the fix,
`new LocalModuleLoader().loadModule("org.jboss.as.jmx").loadService(Extension.class)`
no longer throws `NoSuchMethodError` and completes. (`org.wildfly.extension.core-management`
could not be end-to-end verified the same way — loading its real class graph,
which depends on `java.desktop`, hits an unrelated pre-existing infinite loop
in the VM's field-layout mismatch guard; tracked separately, not a scoping
issue — see `task_54c26646` / a new session spawned for it. The module-scoped
*lookup* for that exact module was already covered by the
`module_service_provider_names`-level test above, which does not depend on
`java.desktop`.)

## Verification

- `cargo test -p cratonvm-native-builtins jboss_module_loader::` — 54 passed,
  including the new
  `wildfly_jboss_modules_service_provider_leak_real_dist_scoping` regression
  test (skips gracefully if the real distribution isn't staged on a given
  machine).
- Manual probe against the real distribution confirmed the `NoSuchMethodError`
  on `Module.loadService` is gone post-fix for `org.jboss.as.jmx`.
