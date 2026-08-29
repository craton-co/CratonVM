# WildFly Module.loadService sees a null moduleClassLoader

Status: ✅ FIXED — fully resolved and verified 2026-07-06 end-to-end against
real HotSpot behavior. The "remaining gap" this doc previously flagged
turned out to be a test-harness artifact, not a live VM bug (see
"Correction" below); a real, separate latent bug found while chasing it down
has also been fixed.

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

## What was fixed (2026-07-06, commit `5c6c6976`)

Direct disassembly of real `jboss-modules.jar` bytecode showed
`org.jboss.modules.Module.loadService(Class)` (the instance method, distinct
from the already-handled static `loadServiceFromCallerModuleLoader`) had
**zero native registration anywhere** — real bytecode ran unmodified. Its
body is `getClass().getModule().addUses(serviceType); return
ServiceLoader.load(serviceType, moduleClassLoader);` — the `addUses` call
walks the JDK's own `java.lang.Module`/`ModuleDescriptor` machinery (a
bookkeeping side effect CratonVM's permissive module model doesn't need),
and only afterward reads `moduleClassLoader`.

1. Added `native_module_load_service` (`../../../../native-builtins/src/jboss_module_loader.rs`)
   reimplementing the observable contract directly (skips `addUses`,
   delegates to `ServiceLoader.load(serviceType, moduleClassLoader)` via
   `native_module_get_class_loader`), registered on `CN_MODULE` and forced
   over real bytecode in both dispatch gates (`force_native_over_real_jdk_bytecode`
   in `interpreter.rs`, `check_override` in `vm_exec.rs`).
2. `ModuleClassLoader.getResources`/`findResources`/`getResource`/`findResource`
   were registered as natives but **not force-listed** in either gate — only
   `findClass` was. Since our synthetic `ModuleClassLoader` instances are
   allocated via `alloc_concurrent_synthetic` (bypassing the real
   constructor), real bytecode's internal `ResourceLoader` state is never
   populated, so these methods silently returned empty results instead of
   the module's own `../../../../apps/META-INF/services/*` entries. Added all four to both
   force-gates alongside `findClass`.

## Correction (2026-07-06, second pass): the "remaining gap" was a test-harness bug, not a VM bug

The first verification pass (same day) built a repro using a bare
`new LocalModuleLoader(new File[] { root })` and found the discovered
provider (`org.jboss.as.jmx.JMXExtension`) still failed to instantiate with
`NoClassDefFoundError`, and documented that as an open residual likely
still causing `WFLYCTL0083`.

Root-caused with `CRATONVM_DIAG_SERVICELOADER=1` + targeted tracing: the
repro never set a module-path root (`-mp` / `CRATONVM_JBOSS_MP_ROOT`), so
CratonVM's dependency-closure walker (`module_visibility_closure` in
`jboss_module_loader.rs`, keyed off `module_path_roots()`) had **no path to
search** for org.jboss.as.controller's declared dependencies (`org.wildfly.common`,
etc.) — every single dependency resolved to `None`, so `org.wildfly.common.Assert`
(needed by `ModelVersion.<clinit>`, needed by `JMXExtension.<clinit>`) could
never be found, and `JMXExtension`'s class state got poisoned to
`InitializationError`.

Setting `CRATONVM_JBOSS_MP_ROOT=<the same modules tree>` — which is the
equivalent of the `-mp` flag real WildFly's `standalone.sh`/`domain.sh`
scripts always pass when they launch `jboss-modules.jar` — made every
dependency resolve correctly, and the full chain
(`loadService` → provider discovery → class resolution → `<clinit>` →
`newInstance()`) completed cleanly, matching HotSpot exactly
(`count=1`, `Found extension: org.jboss.as.jmx.JMXExtension`). Since the
real WildFly suite runner replicates WildFly's actual launch command
(which always includes `-mp`), this specific `NoClassDefFoundError` chain
was never reproducible in the real `HostExcludesTestCase` — it was an
artifact of the simplified test harness, not a CratonVM defect.

## Real latent bug found and fixed along the way

While chasing the false residual, found a genuine (if narrower) bug in
`load_provider_class` (`../../../../native-builtins/src/service_loader.rs`): it tried
the context-free 1-arg `Class.forName(fqn)` **before** the loader-scoped
`loader.findClass`/`loader.loadClass` path. `Class.forName(fqn)` (1-arg)
resolves via whatever classloader CratonVM treats as the "caller" for a
native-invoked call — not the loader ServiceLoader was actually given. If
that wrong-context attempt manages to define the class but its `<clinit>`
fails (e.g. because a dependency only visible through the *correct* loader
can't be found from the wrong context), `ClassState` is permanently poisoned
to `InitializationError` — JVM class state never resets. A later, correct
resolution via the right loader then returns the mirror for that SAME
already-poisoned `ClassId` (`Class.forName`/`loadClass` return the existing
class regardless of which loader asks), so `load_provider_class` reports
success but `Constructor.newInstance()` throws `NoClassDefFoundError` on
first real use — exactly the symptom that looked like a residual bug above.

Fixed by reordering `load_provider_class` to try the loader-scoped path
first when a loader is known, falling back to the context-free scan only
if no loader was given or the loader couldn't resolve it. This is a
narrower, more general hardening fix independent of the `-mp` test-harness
gap: even in a correctly-configured environment, a class whose `<clinit>`
depends on loader-scoped visibility could still have been poisoned by
trying the wrong context first. Verified: the misconfigured (no `-mp`)
scenario now fails with a normal, visible `NoSuchMethodError` (missing
classpath entry — an expected, legitimate failure) instead of a silently
swallowed poisoned-class `NoClassDefFoundError`.

Also hardened `native_sl_iterator`: a `Constructor.newInstance()` failure
that is a VM-internal error (`MethodCallFailed::InternalError`, e.g.
`Linkage`/`NoClassDefFoundError`) is now surfaced via `tracing::warn!`
instead of being silently treated identically to "provider not found" —
this is exactly the class of bug that made the false residual hard to spot
in the first place. Legitimate "provider not found"/"no zero-arg
constructor" cases still skip silently (unchanged, to avoid a broad
behavior change across other suites that may rely on lenient handling).

## Verification

Repro used the **real** `jboss-modules.jar` + a fully extracted
`wildfly-32.0.1.Final.zip` module tree (not hand-mocked stand-ins), driving
the exact call shape from
`org.jboss.as.controller.parsing.DeferredExtensionContext` (confirmed via
disassembly to be a real caller of the instance `loadService`):
`moduleLoader.loadModule("org.jboss.as.jmx").loadService(Extension.class)`,
then iterating the result exactly as WildFly's extension-loading code does.

With `CRATONVM_JBOSS_MP_ROOT` set to the module tree: `count=1`,
`Found extension: org.jboss.as.jmx.JMXExtension` — matches HotSpot exactly,
both before and after the `load_provider_class` reordering fix. Plain
classpath `ServiceLoader` usage (non-JBoss, `../../../../apps/META-INF/services`-based)
re-verified unaffected by the reordering: `count=1` matching HotSpot.
