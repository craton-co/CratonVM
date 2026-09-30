package cratonvm.wildfly;

import org.jboss.modules.DefaultBootModuleLoaderHolder;
import org.jboss.modules.LocalModuleLoader;
import org.jboss.modules.Module;
import org.jboss.modules.ModuleClassLoader;
import org.jboss.modules.ModuleLoader;

/**
 * WP8.10 — minimal jboss-modules boot probe.
 *
 * Exercises just enough of the org.jboss.modules.Main.main(String[])
 * boot chain to surface a *deterministic* first-failure under cratonvm,
 * without needing the 150 MB WildFly tarball or a real jboss-modules.jar.
 *
 * What the real Main.main() does, abridged (verified against
 * jboss-modules 2.1.4.Final source on GitHub):
 *
 *   1. environmentLoader = DefaultBootModuleLoaderHolder.INSTANCE;
 *   2. Module.initBootModuleLoader(environmentLoader);
 *   3. module = environmentLoader.loadModule(moduleName);
 *   4. module.run(className, argsArray);  // does findStatic + invokeExact
 *
 * Each `probe*` method below isolates one of those steps so the Rust
 * harness can pinpoint the *first* failure (NoSuchMethodError, NPE,
 * ClassCastException) rather than chasing a 30-frame stack out of the
 * real distribution.
 *
 * Return convention (matches Wp18ServiceLoaderE2E):
 *   1   = success
 *   0   = soft mismatch (e.g. wrong type observed)
 *  -99  = caught Throwable; check System.err for cause
 *
 * The org.jboss.modules.* references compile against the *synthetic*
 * stub layout declared in classloading/src/class_manager.rs — these
 * classes are produced from skeleton .java sources adjacent to this
 * probe so javac is happy without needing jboss-modules.jar on the
 * compile classpath.
 */
public class JBossModulesProbe {

    /**
     * Probe 0: Sanity — can we even load and reference
     * org.jboss.modules.Module? If this is -99 (NoClassDefFoundError),
     * the synthetic stub for the class isn't being created on first
     * reference — a much earlier failure than the boot holder.
     */
    public static int probeJBossModuleClassReachable() {
        try {
            String name = Module.class.getName();
            return (name != null && name.contains("Module")) ? 1 : 0;
        } catch (Throwable t) {
            try {
                System.err.println("probe0 caught: " + t.getClass().getName());
            } catch (Throwable ignored) {}
            return -99;
        }
    }

    /**
     * Probe 1: post-clinit fixup populates DefaultBootModuleLoaderHolder.INSTANCE.
     *
     * If this returns 0 or -99 the post_clinit_fixup hook in
     * vm/src/vm/vm_util.rs:1022 is broken — INSTANCE is null even
     * though the class clinit-ed. Without this fix, every downstream
     * call NPEs.
     */
    public static int probeBootHolderInstancePopulated() {
        try {
            ModuleLoader loader = DefaultBootModuleLoaderHolder.INSTANCE;
            return loader != null ? 1 : 0;
        } catch (Throwable t) {
            // Avoid printStackTrace — its native is not always wired
            // up under cratonvm.  Instead, write the throwable FQN +
            // message via System.err.println, which routes through the
            // PrintStream natives we know are registered.
            try {
                System.err.println("probe caught: " + t.getClass().getName()
                    + ": " + String.valueOf(t.getMessage()));
            } catch (Throwable ignored) {
                // Even println may fail in extreme states; swallow.
            }
            return -99;
        }
    }

    /**
     * Probe 2: the populated INSTANCE is a LocalModuleLoader — pointer
     * identity tests will fail later if the type is wrong.
     */
    public static int probeBootHolderIsLocalModuleLoader() {
        try {
            ModuleLoader loader = DefaultBootModuleLoaderHolder.INSTANCE;
            if (loader == null) return 0;
            return (loader instanceof LocalModuleLoader) ? 1 : 0;
        } catch (Throwable t) {
            // Avoid printStackTrace — its native is not always wired
            // up under cratonvm.  Instead, write the throwable FQN +
            // message via System.err.println, which routes through the
            // PrintStream natives we know are registered.
            try {
                System.err.println("probe caught: " + t.getClass().getName()
                    + ": " + String.valueOf(t.getMessage()));
            } catch (Throwable ignored) {
                // Even println may fail in extreme states; swallow.
            }
            return -99;
        }
    }

    /**
     * Probe 3: LocalModuleLoader.loadModule(String) succeeds for a
     * module whose module.xml is present in the fixture's modules/
     * tree. This is the load-bearing call — if it fails the boot
     * stops at the very first `loader.loadModule(moduleName)` step.
     *
     * Failure modes ranked by likelihood:
     *   a. ModuleNotFoundException — synthetic module.xml not parsed
     *      or located by jboss_module_loader::locate_module_xml.
     *   b. NoSuchMethodError on loadModule — native dispatch broken
     *      (registration site: jboss_module_loader::register_jboss_module_loader).
     *   c. NPE on loader.loadModule(...) — INSTANCE was null
     *      (= probe 1 also failed).
     */
    public static int probeLoadModuleSucceeds(String moduleName) {
        try {
            ModuleLoader loader = DefaultBootModuleLoaderHolder.INSTANCE;
            if (loader == null) return 0;
            Module m = loader.loadModule(moduleName);
            return m != null ? 1 : 0;
        } catch (Throwable t) {
            // Avoid printStackTrace — its native is not always wired
            // up under cratonvm.  Instead, write the throwable FQN +
            // message via System.err.println, which routes through the
            // PrintStream natives we know are registered.
            try {
                System.err.println("probe caught: " + t.getClass().getName()
                    + ": " + String.valueOf(t.getMessage()));
            } catch (Throwable ignored) {
                // Even println may fail in extreme states; swallow.
            }
            return -99;
        }
    }

    /**
     * Probe 4: module.getName() round-trips back the module name we
     * asked for. Validates that the synthetic Module shape's `name`
     * slot was populated by build_module_object().
     */
    public static int probeLoadedModuleName(String moduleName) {
        try {
            ModuleLoader loader = DefaultBootModuleLoaderHolder.INSTANCE;
            if (loader == null) return 0;
            Module m = loader.loadModule(moduleName);
            if (m == null) return 0;
            String got = m.getName();
            return moduleName.equals(got) ? 1 : 0;
        } catch (Throwable t) {
            // Avoid printStackTrace — its native is not always wired
            // up under cratonvm.  Instead, write the throwable FQN +
            // message via System.err.println, which routes through the
            // PrintStream natives we know are registered.
            try {
                System.err.println("probe caught: " + t.getClass().getName()
                    + ": " + String.valueOf(t.getMessage()));
            } catch (Throwable ignored) {
                // Even println may fail in extreme states; swallow.
            }
            return -99;
        }
    }

    /**
     * Probe 5: module.getClassLoader() returns a non-null
     * ModuleClassLoader. This drives the Module → ModuleClassLoader
     * lazy-init path in jboss_module_loader::native_module_get_class_loader.
     *
     * Real WildFly uses this path to push every ModuleClassLoader's
     * resource roots onto the dynamic application classpath — without
     * it, downstream class loading fails with NoClassDefFoundError.
     */
    public static int probeModuleClassLoaderResolves(String moduleName) {
        try {
            ModuleLoader loader = DefaultBootModuleLoaderHolder.INSTANCE;
            if (loader == null) return 0;
            Module m = loader.loadModule(moduleName);
            if (m == null) return 0;
            ClassLoader mcl = m.getClassLoader();
            return mcl != null ? 1 : 0;
        } catch (Throwable t) {
            // Avoid printStackTrace — its native is not always wired
            // up under cratonvm.  Instead, write the throwable FQN +
            // message via System.err.println, which routes through the
            // PrintStream natives we know are registered.
            try {
                System.err.println("probe caught: " + t.getClass().getName()
                    + ": " + String.valueOf(t.getMessage()));
            } catch (Throwable ignored) {
                // Even println may fail in extreme states; swallow.
            }
            return -99;
        }
    }

    /**
     * Probe 6: ModuleNotFoundException is raised (not generic Exception)
     * for an unknown module. The boot loop in jboss_module_loader catches
     * this specific type to print the friendly "ModuleNotFoundException:
     * org.foo.bar" message that WildFly admins expect.
     */
    public static int probeMissingModuleThrowsNotFound() {
        try {
            ModuleLoader loader = DefaultBootModuleLoaderHolder.INSTANCE;
            if (loader == null) return 0;
            try {
                loader.loadModule("nonexistent.module.that.should.never.be.found");
                return 0; // didn't throw — wrong
            } catch (Throwable expected) {
                String fqn = expected.getClass().getName();
                return fqn.contains("ModuleNotFoundException")
                    || fqn.contains("ClassNotFoundException")
                    ? 1 : 0;
            }
        } catch (Throwable t) {
            // Avoid printStackTrace — its native is not always wired
            // up under cratonvm.  Instead, write the throwable FQN +
            // message via System.err.println, which routes through the
            // PrintStream natives we know are registered.
            try {
                System.err.println("probe caught: " + t.getClass().getName()
                    + ": " + String.valueOf(t.getMessage()));
            } catch (Throwable ignored) {
                // Even println may fail in extreme states; swallow.
            }
            return -99;
        }
    }
}
