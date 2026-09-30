// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L3: a transformer that overrides only
// the JDK 9+ `transform(Module, ClassLoader, String, Class, ProtectionDomain,
// byte[])` form is run, and handed the module HotSpot hands it; a
// transformer that throws does not stop the next one.
//
// HotSpot runs every transformer through `TransformerManager.transform`,
// which calls the SIX-argument form (whose default body calls the
// five-argument one) and catches whatever a transformer throws. The module is
// the class's own on a retransformation, and at load the named module that
// holds the package, else the defining loader's unnamed module
// (`InstrumentationImpl.transform`). CratonVM's chain walk
// (`instrument::run_chain_over_bytes`) called the five-argument form
// directly, so a transformer overriding only the `Module` form never ran.
//
// Rows:
//   load        -- `Loaded`'s first load: the module the `Module`-form
//                  transformer saw (`loader-unnamed` = the application
//                  loader's unnamed module; `none` = never called) and how
//                  often the throwing transformer ran;
//   retransform -- the same after `retransformClasses(Loaded)`;
//   jdk         -- a JDK class loaded after the transformers were added
//                  (`java.sql.JDBCType`, in the named module `java.sql`).
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     load module=loader-unnamed thrower=1 value=1
//     retransform module=loader-unnamed thrower=2 value=1
//     jdk module=named:java.sql
// CratonVM before wave 42 (read from the code, not run; both modes):
//     load module=none thrower=1 value=1
//     retransform module=none thrower=2 value=1
//     jdk module=none
// Wave 42 calls the six-argument form (`instrument::transform_module` builds
// the module). The `jdk` row also needs the load-time hook to offer a JDK
// class loaded after `premain` to the transformers
// (`instrument::pre_transform_for_load`), which this probe is the first to
// ask; the orchestrator's first run says whether it does.
//
// Positive control: `CRATONVM_DBG_RETRANSFORM=1` prints
//     [RETRANSFORM]   transform(Module, ...) for L3W42TransformerModuleForm$Loaded: module=present
// once per chain walk over `Loaded` (load and retransform).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W42TransformerModuleForm$Agent
//     Can-Retransform-Classes: true
// containing L3W42TransformerModuleForm*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W42TransformerModuleForm
// Without the agent both VMs print "no agent".
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W42TransformerModuleForm {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Loaded {
        public static int value() {
            return 1;
        }
    }

    static final String LOADED = "L3W42TransformerModuleForm$Loaded";
    static final String JDK = "java/sql/JDBCType";

    static volatile String loadSeen = "none";
    static volatile String retransformSeen = "none";
    static volatile String jdkSeen = "none";
    static volatile int throwerCalls;

    static String describe(Module module, ClassLoader loader) {
        if (module == null) {
            return "null";
        }
        if (module.isNamed()) {
            return "named:" + module.getName();
        }
        Module unnamed = loader == null ? null : loader.getUnnamedModule();
        return module == unnamed ? "loader-unnamed" : "other-unnamed";
    }

    /** Overrides only the JDK 9+ `Module` form. */
    static final class ModuleOnly implements ClassFileTransformer {
        @Override
        public byte[] transform(Module module, ClassLoader loader, String name, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (LOADED.equals(name)) {
                String d = describe(module, loader);
                if (redefined == null) {
                    loadSeen = d;
                } else {
                    retransformSeen = d;
                }
            } else if (JDK.equals(name)) {
                jdkSeen = describe(module, loader);
            }
            return null;
        }
    }

    /** Throws for `Loaded`: the next transformer still runs. */
    static final class Thrower implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String name, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (LOADED.equals(name)) {
                throwerCalls++;
                throw new IllegalStateException("thrower");
            }
            return null;
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Thrower(), true);
        i.addTransformer(new ModuleOnly(), true);
        int v = Loaded.value();
        System.out.println("load module=" + loadSeen + " thrower=" + throwerCalls + " value=" + v);
        i.retransformClasses(Loaded.class);
        System.out.println("retransform module=" + retransformSeen + " thrower=" + throwerCalls
                + " value=" + Loaded.value());
        Class.forName("java.sql.JDBCType");
        System.out.println("jdk module=" + jdkSeen);
    }
}
