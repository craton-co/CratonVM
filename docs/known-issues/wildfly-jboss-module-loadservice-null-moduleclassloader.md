# WildFly Module.loadService sees a null moduleClassLoader

Status: partially fixed — two real gaps closed 2026-07-06, but the underlying
`WFLYCTL0083: Failed to load module org.jboss.as.jmx` symptom likely still
reproduces via a different, deeper cause (see "Remaining gap" below). Kept in
`docs/known-issues/` per the "still-open sub-part" convention.

Observed while rerunning `HostExcludesTestCase` on the Azure WildFly runner
after scoping JBoss Modules service-provider resources.

## Reproduction

Run:

```bash
./run-suite.sh run --category failed --only HostExcludesTestCase --count 1 --jit on --class-to 300 --tag azure-hostexcludes-svcscope-008
```

Failure:

```text
WFLYCTL0083: Failed to load module org.jboss.as.jmx
```

## What was fixed (2026-07-06)

Direct disassembly of real `jboss-modules.jar` bytecode showed
`org.jboss.modules.Module.loadService(Class)` (the instance method, distinct
from the already-handled static `loadServiceFromCallerModuleLoader`) had
**zero native registration anywhere** — real bytecode ran unmodified. Its
body is `getClass().getModule().addUses(serviceType); return
ServiceLoader.load(serviceType, moduleClassLoader);` — the `addUses` call
walks the JDK's own `java.lang.Module`/`ModuleDescriptor` machinery (a
bookkeeping side effect CratonVM's permissive module model doesn't need),
and only afterward reads `moduleClassLoader`.

Fixed in `fix/jbossmodule-loadservice-nonull`:
1. Added `native_module_load_service` (`native-builtins/src/jboss_module_loader.rs`)
   reimplementing the observable contract directly (skips `addUses`,
   delegates to `ServiceLoader.load(serviceType, moduleClassLoader)` via
   `native_module_get_class_loader`), registered on `CN_MODULE` and forced
   over real bytecode in both dispatch gates (`force_native_over_real_jdk_bytecode`
   in `interpreter.rs`, `check_override` in `vm_exec.rs`).
2. While building a faithful repro (see below), found `ModuleClassLoader.getResources`/
   `findResources`/`getResource`/`findResource` were registered as natives
   but **not force-listed** in either gate — only `findClass` was. Since our
   synthetic `ModuleClassLoader` instances are allocated via
   `alloc_concurrent_synthetic` (bypassing the real constructor), real
   bytecode's internal `ResourceLoader` state is never populated, so these
   methods silently returned empty results instead of the module's own
   `META-INF/services/*` entries. Added all four to both force-gates
   alongside `findClass`.

## Verification

Repro used the **real** `jboss-modules.jar` + a fully extracted
`wildfly-32.0.1.Final.zip` module tree (not hand-mocked stand-ins), driving
the exact call shape from
`org.jboss.as.controller.parsing.DeferredExtensionContext` (confirmed via
disassembly to be a real caller of the instance `loadService`):
`moduleLoader.loadModule("org.jboss.as.jmx").loadService(Extension.class)`.

Before the fix: no crash (contrary to this doc's original NPE hypothesis —
`moduleClassLoader` is read *after* the harmless `addUses` call and
`ServiceLoader.load` tolerates a null loader), but silently discovered **0**
providers vs. HotSpot's 1 (`org.jboss.as.jmx.JMXExtension`, registered via
`META-INF/services/org.jboss.as.controller.Extension` in
`wildfly-jmx-24.0.1.Final.jar`).

After the fix: provider *discovery* now correctly finds 1 provider
(`[SL-LOADER-DBG] ... providers=1 (["org.jboss.as.jmx.JMXExtension"])`,
confirmed via `CRATONVM_DIAG_SERVICELOADER=1`), matching HotSpot.

## Remaining gap (new, more precisely diagnosed)

Even after the fix, the discovered provider still fails to instantiate:
`native_sl_iterator` (`native-builtins/src/service_loader.rs`) logs
`skip (newInstance returned null): org.jboss.as.jmx.JMXExtension` and
`Constructor.newInstance()`'s error is silently swallowed via `.ok()`
instead of propagating (real JDK wraps constructor failures in
`ServiceConfigurationError` and surfaces them). The underlying error was
`NoClassDefFoundError { class_name: "org/jboss/as/jmx/JMXExtension" }` —
the class had already reached `ClassState::InitializationError` from an
earlier failed resolution attempt inside `load_provider_class`'s first
(1-arg, caller-context) `Class.forName` try, before the loader-scoped retry
picked it back up. A minimal isolated repro
(`Class.forName("org.jboss.as.jmx.JMXExtension", true, moduleClassLoader)`,
no ServiceLoader involved) gets a *different* symptom
(`ClassNotFoundException`), suggesting real inconsistency in how
`org.jboss.modules.ModuleClassLoader`-loaded classes get resolved/initialized
depending on call path.

Net effect: `ServiceLoader.load(...).iterator()` still yields 0 usable
providers end-to-end for real WildFly extension modules, so
`WFLYCTL0083: Failed to load module org.jboss.as.jmx` likely still
reproduces in the real `HostExcludesTestCase` — this doc stays open for that
residual. Follow-up should: (a) stop swallowing `Constructor.newInstance()`
errors in `native_sl_iterator` (surface them instead, matching
`ServiceConfigurationError` semantics), and (b) reconcile why
`Class.forName`/`ClassLoader.loadClass` for a `ModuleClassLoader`-visible
class behaves inconsistently across call paths (single-arg `forName` vs.
3-arg `forName` vs. `loader.loadClass` vs. ServiceLoader's internal retry).
