// Interpreter round i1, wave 28, lane L5 — does a class load through a user
// class loader hold the loader's own monitor?
//
// JDK 25 / HotSpot: a loader class that did not call
// `registerAsParallelCapable()` gets `parallelLockMap == null` from
// `ClassLoader.<init>`, so
//   * the JDK's `loadClass(String,boolean)` runs under
//     `synchronized (getClassLoadingLock(name))`, which is the loader itself
//     (rows `... findClass holds loader`), and
//   * a VM-initiated load (resolving a reference from a class the loader
//     defined) holds the loader's monitor around the call to its public
//     `loadClass(String)` (`SystemDictionary::resolve_instance_class_or_null`,
//     `ObjectLocker`; rows `... vm load holds loader`).
// A registered (parallel-capable) loader gets neither: per-name locks in
// `loadClass`, no VM lock.
//
// `Plain`/`Parallel` override `loadClass(String)` WITHOUT synchronizing and
// define `Ref`/`Dep` themselves, so the only monitor the `Dep` request can see
// is the one the VM took. `FindPlain`/`FindParallel` override only `findClass`
// (platform parent, so delegation misses and `findClass` runs) and are asked
// from Java, so the monitor is the one the base `loadClass` took.
//
// CratonVM paths (wave 28): the VM row is
// `constants.rs` `resolve_class_loader_aware` -> `drive_defining_loader_load`
// -> `drive_defining_loader_load_named` (monitor when
// `classloader_real::loader_locks_itself`); the findClass row is the base
// `ClassLoader.loadClass` native `classloader_real.rs`
// `cl_real_load_class` -> `cl_real_load_class_base` (same predicate) ->
// `cl_real_load_class_base_rooted` -> the `findClass` override.
//
// Run (no setup):
//   javac -d out L5W28LoaderMonitorAtLoad.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W28LoaderMonitorAtLoad
//
// Expected HotSpot 25 output (compare verbatim):
//   plain vm load holds loader: true
//   parallel vm load holds loader: false
//   plain findClass holds loader: true
//   parallel findClass holds loader: false
//
// CratonVM before wave 28 (from the code, not run): rows 1 and 3 `false`
// (no VM lock, a native base `loadClass` that took no lock, and a
// `parallelLockMap` allocated for every loader). `--compatible` is expected to
// keep printing `false` for rows 1 and 3: the change is `--jdk-only` only
// (that mode's `ParallelLoaders` set does not hold the registrations).

import java.io.ByteArrayOutputStream;
import java.io.InputStream;

public class L5W28LoaderMonitorAtLoad {
    public static class Ref {
        public static Object run() {
            return new Dep();
        }
    }

    public static class Dep {
    }

    static final String REF = "L5W28LoaderMonitorAtLoad$Ref";
    static final String DEP = "L5W28LoaderMonitorAtLoad$Dep";

    static byte[] bytes(String name) throws ClassNotFoundException {
        try (InputStream in = L5W28LoaderMonitorAtLoad.class.getResourceAsStream("/" + name + ".class")) {
            if (in == null) {
                throw new ClassNotFoundException(name);
            }
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            for (int n; (n = in.read(buf)) > 0; ) {
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        } catch (java.io.IOException e) {
            throw new ClassNotFoundException(name, e);
        }
    }

    // Both loader pairs extend ClassLoader directly: registration needs the
    // superclass registered (`ParallelLoaders.register`), so a registered
    // subclass of an unregistered loader would not be parallel-capable.
    interface Recording {
        String held();
    }

    // Overrides the public loadClass(String) the VM calls; takes no lock.
    static final class Plain extends ClassLoader implements Recording {
        volatile String depHeld = "not asked";

        Plain() {
            super(ClassLoader.getPlatformClassLoader());
        }

        public String held() {
            return depHeld;
        }

        @Override
        public Class<?> loadClass(String name) throws ClassNotFoundException {
            if (!name.equals(REF) && !name.equals(DEP)) {
                return super.loadClass(name);
            }
            if (name.equals(DEP)) {
                depHeld = String.valueOf(Thread.holdsLock(this));
            }
            Class<?> c = findLoadedClass(name);
            if (c == null) {
                byte[] b = bytes(name);
                c = defineClass(name, b, 0, b.length);
            }
            return c;
        }
    }

    static final class Parallel extends ClassLoader implements Recording {
        static {
            registerAsParallelCapable();
        }

        volatile String depHeld = "not asked";

        Parallel() {
            super(ClassLoader.getPlatformClassLoader());
        }

        public String held() {
            return depHeld;
        }

        @Override
        public Class<?> loadClass(String name) throws ClassNotFoundException {
            if (!name.equals(REF) && !name.equals(DEP)) {
                return super.loadClass(name);
            }
            if (name.equals(DEP)) {
                depHeld = String.valueOf(Thread.holdsLock(this));
            }
            Class<?> c = findLoadedClass(name);
            if (c == null) {
                byte[] b = bytes(name);
                c = defineClass(name, b, 0, b.length);
            }
            return c;
        }
    }

    // Override only findClass; the base loadClass decides the lock.
    static final class FindPlain extends ClassLoader implements Recording {
        volatile String held = "not asked";

        FindPlain() {
            super(ClassLoader.getPlatformClassLoader());
        }

        public String held() {
            return held;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals(REF)) {
                throw new ClassNotFoundException(name);
            }
            held = String.valueOf(Thread.holdsLock(this));
            byte[] b = bytes(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    static final class FindParallel extends ClassLoader implements Recording {
        static {
            registerAsParallelCapable();
        }

        volatile String held = "not asked";

        FindParallel() {
            super(ClassLoader.getPlatformClassLoader());
        }

        public String held() {
            return held;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals(REF)) {
                throw new ClassNotFoundException(name);
            }
            held = String.valueOf(Thread.holdsLock(this));
            byte[] b = bytes(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    static <L extends ClassLoader & Recording> String vmLoad(L loader) throws Exception {
        Class<?> ref = loader.loadClass(REF);
        ref.getMethod("run").invoke(null);
        return loader.held();
    }

    static <L extends ClassLoader & Recording> String findLoad(L loader) throws Exception {
        loader.loadClass(REF);
        return loader.held();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("plain vm load holds loader: " + vmLoad(new Plain()));
        System.out.println("parallel vm load holds loader: " + vmLoad(new Parallel()));
        System.out.println("plain findClass holds loader: " + findLoad(new FindPlain()));
        System.out.println("parallel findClass holds loader: " + findLoad(new FindParallel()));
    }
}
