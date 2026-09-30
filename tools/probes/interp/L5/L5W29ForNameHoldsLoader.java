// Interpreter round i1, wave 29, lane L5 -- `Class.forName(name, init, loader)`
// calls the loader's `loadClass` with the loader's monitor held when the loader
// is not parallel-capable, as every VM-initiated load does on HotSpot
// (`JVM_FindClassFromCaller` -> `SystemDictionary::resolve_instance_class_or_null`,
// `ObjectLocker` on the loader); a parallel-capable loader is called unlocked.
//
// Each loader overrides the PUBLIC `loadClass(String)` without synchronizing,
// and records `Thread.holdsLock(this)` when asked for the probe's target.
//
// CratonVM before wave 29 (from the code, not run): the `Class.forName` native
// (`native-builtins/src/lang_class.rs` `native_class_for_name`) called
// `loadClass` with no lock, so `plain forName holds loader: false`
// (`docs/internal/fixed-bugs/interpreter-L5-a-racing-resolution-of-one-class-entry-can-leave-threads-disagreeing-FIXED-20261005.md`,
// the native doors). Wave 29 routes it, and the other natives that load
// through a user loader on the VM's behalf, through
// `classloader_real::invoke_load_class_as_the_vm`. `--compatible` is unchanged
// (every loader keeps a `parallelLockMap` there): `false` on both rows.
//
// Run (no setup):
//   javac -d out L5W29ForNameHoldsLoader.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W29ForNameHoldsLoader
//
// Expected HotSpot 25 output (compare verbatim):
//   plain forName holds loader: true
//   parallel forName holds loader: false

public class L5W29ForNameHoldsLoader {
    static final String TARGET = "L5W29ForNameHoldsLoader$Target";

    static class Plain extends ClassLoader {
        volatile String held = "not asked";

        Plain(ClassLoader parent) {
            super(parent);
        }

        @Override
        public Class<?> loadClass(String name) throws ClassNotFoundException {
            if (name.equals(TARGET)) {
                held = String.valueOf(Thread.holdsLock(this));
            }
            return super.loadClass(name);
        }
    }

    static class Parallel extends ClassLoader {
        static {
            registerAsParallelCapable();
        }

        volatile String held = "not asked";

        Parallel(ClassLoader parent) {
            super(parent);
        }

        @Override
        public Class<?> loadClass(String name) throws ClassNotFoundException {
            if (name.equals(TARGET)) {
                held = String.valueOf(Thread.holdsLock(this));
            }
            return super.loadClass(name);
        }
    }

    public static class Target {
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W29ForNameHoldsLoader.class.getClassLoader();
        Plain plain = new Plain(app);
        Class.forName(TARGET, false, plain);
        System.out.println("plain forName holds loader: " + plain.held);
        Parallel parallel = new Parallel(app);
        Class.forName(TARGET, false, parallel);
        System.out.println("parallel forName holds loader: " + parallel.held);
    }
}
