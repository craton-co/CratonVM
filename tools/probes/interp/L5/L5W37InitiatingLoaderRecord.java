// Interpreter round i1, wave 37, lane L5 -- JVMS §5.3.2 / §5.3.5: a loader
// that INITIATED a class (the VM resolved a name through it and it delegated,
// or `Class.forName(name, init, loader)`) has that class in its namespace:
// `findLoadedClass` returns it, and the loader defining the name itself is a
// duplicate-definition `LinkageError`. A plain Java `loader.loadClass(name)`
// that delegates does NOT make the loader an initiating loader.
//
// Evidence for `docs/internal/fixed-bugs/interpreter-L5-an-initiating-loader-record-is-invisible-to-findloadedclass-and-define-FIXED-20261003.md`
// (open, not fixed in wave 37).
//
// CratonVM (from the code, not run; `--jdk-only`): the initiating record is
// only the VM's capped per-loader memo (`initiating_resolution_cache`), which
// neither `findLoadedClass0` (`find_loaded_class_for_loader`: classes this
// loader DEFINED) nor the define path (`ClassManager`'s duplicate check over
// `loaded_classes`) consults, and `Class.forName` records nothing, so the rows
// are expected to read `define after vm-initiated: ok loader=l1`,
// `findLoadedClass after vm-initiated=null`, `define after forName: ok
// loader=l3`.
//
// Wave 38 (lane L5), `--jdk-only`: the record is VM state
// (`ClassRealm::initiating_records`, written by the class-resolution door and
// by `Class.forName` with a loader), `findLoadedClass0` answers from it after
// the loader's own definitions, and the define backend refuses the loader's
// own definition of a recorded name with HotSpot's message: HotSpot's rows
// expected. Positive control: `CRATONVM_DBG=access` prints two
// `[ACCESS-DBG] INITIATING-RECORD DENY` lines and, at exit,
// `[ACCESS-DBG] LOADER census: ... initiating-findloaded=1 initiating-define-denied=2`
// (`initiating-findloaded` counts every `findLoadedClass` the record answers,
// including the loader's own calls inside `loadClass`, so it may read more).
// `--compatible` keeps the rows above by design.
//
// Run (no setup):
//   javac -d out L5W37InitiatingLoaderRecord.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W37InitiatingLoaderRecord
//
// Expected HotSpot 25.0.3 output (compare verbatim):
//   vm-initiated dep loader=app
//   define after vm-initiated: java.lang.LinkageError: loader 'l1' @H attempted duplicate class definition for L5W37InitiatingLoaderRecord$Dep. (L5W37InitiatingLoaderRecord$Dep is in unnamed module of loader 'l1' @H, parent loader 'app')
//   findLoadedClass after vm-initiated=found
//   define after java loadClass: ok loader=l2
//   define after forName: java.lang.LinkageError: loader 'l3' @H attempted duplicate class definition for L5W37InitiatingLoaderRecord$Dep. (L5W37InitiatingLoaderRecord$Dep is in unnamed module of loader 'l3' @H, parent loader 'app')

import java.io.IOException;
import java.io.InputStream;
import java.util.Set;

public class L5W37InitiatingLoaderRecord {
    static final String P = "L5W37InitiatingLoaderRecord$";

    static final class L extends ClassLoader {
        final Set<String> own;

        L(String n, Set<String> own) {
            super(n, L5W37InitiatingLoaderRecord.class.getClassLoader());
            this.own = own;
        }

        @Override
        protected Class<?> loadClass(String name, boolean r) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (!own.contains(name)) {
                    return super.loadClass(name, r);
                }
                return def(name);
            }
        }

        Class<?> loaded(String n) {
            return findLoadedClass(n);
        }

        Class<?> def(String name) throws ClassNotFoundException {
            try (InputStream in =
                    ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            } catch (IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + ": "
                + String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H");
    }

    public static void main(String[] a) throws Exception {
        // VM-initiated: `User` (l1's own) resolves `Dep`, which l1 delegates.
        L l1 = new L("l1", Set.of(P + "User"));
        Object r = l1.loadClass(P + "User").getMethod("make").invoke(null);
        System.out.println("vm-initiated dep loader="
                + (r.getClass().getClassLoader() == l1 ? "l1" : "app"));
        try {
            Class<?> c = l1.def(P + "Dep");
            System.out.println("define after vm-initiated: ok loader="
                    + (c.getClassLoader() == l1 ? "l1" : "other"));
        } catch (LinkageError e) {
            System.out.println("define after vm-initiated: " + describe(e));
        }
        System.out.println("findLoadedClass after vm-initiated="
                + (l1.loaded(P + "Dep") == null ? "null" : "found"));
        // A Java `loadClass` that delegates records nothing.
        L l2 = new L("l2", Set.of());
        l2.loadClass(P + "Dep");
        try {
            Class<?> c = l2.def(P + "Dep");
            System.out.println("define after java loadClass: ok loader="
                    + (c.getClassLoader() == l2 ? "l2" : "other"));
        } catch (LinkageError e) {
            System.out.println("define after java loadClass: " + describe(e));
        }
        // `Class.forName` with the loader initiates.
        L l3 = new L("l3", Set.of());
        Class.forName(P + "Dep", false, l3);
        try {
            Class<?> c = l3.def(P + "Dep");
            System.out.println("define after forName: ok loader="
                    + (c.getClassLoader() == l3 ? "l3" : "other"));
        } catch (LinkageError e) {
            System.out.println("define after forName: " + describe(e));
        }
    }

    public static class Dep {
    }

    public static class User {
        public static Object make() {
            return new Dep();
        }
    }
}
