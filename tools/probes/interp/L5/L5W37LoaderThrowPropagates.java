// Interpreter round i1, wave 37, lane L5 -- what a VM-initiated resolution
// does with an exception the initiating loader's `loadClass` throws. JVMS
// §5.3: a `ClassNotFoundException` becomes `NoClassDefFoundError` (cause: the
// CNFE); HotSpot propagates any OTHER throwable as it is. The loader here
// throws for a name that IS on the application class path, so a VM that
// swallows the throw and asks someone else finds a class.
//
// Evidence for `docs/internal/fixed-bugs/interpreter-L5-a-vm-initiated-loadclass-exception-is-swallowed-into-a-global-fallback-FIXED-20261005.md`
// (open, not fixed in wave 37; the loader-constraint `LinkageError` alone
// propagates since wave 37).
//
// CratonVM before wave 38 (from the code): `drive_defining_loader_load_named`
// treated every failed `loadClass` as "not answered" and
// `resolve_class_loader_aware` fell back to the global store, which loads the
// name from the class path: `ise=ok`, `error=ok`, `linkage=ok`, `cnfe=ok`.
// Wave 38 (lane L5) propagates the throw under `--jdk-only` for a loader whose
// `loadClass` is its own bytecode (this one): HotSpot's rows expected, with
// `CRATONVM_DBG=access` printing four `[ACCESS-DBG] LOADER-THROW PROPAGATE`
// lines. `--compatible` keeps `ok` on every row by design.
//
// Run (no setup):
//   javac -d out L5W37LoaderThrowPropagates.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W37LoaderThrowPropagates
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`; compare verbatim):
//   ise=java.lang.IllegalStateException cause=null
//   error=java.lang.AssertionError cause=null
//   linkage=java.lang.LinkageError cause=null
//   cnfe=java.lang.NoClassDefFoundError cause=java.lang.ClassNotFoundException

import java.io.IOException;
import java.io.InputStream;

public class L5W37LoaderThrowPropagates {
    static final String P = "L5W37LoaderThrowPropagates$";

    static final class Throwing extends ClassLoader {
        final String kind;

        Throwing(String kind) {
            super(L5W37LoaderThrowPropagates.class.getClassLoader());
            this.kind = kind;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.equals(P + "Target")) {
                    switch (kind) {
                        case "ise":
                            throw new IllegalStateException("refused " + name);
                        case "error":
                            throw new AssertionError("refused " + name);
                        case "linkage":
                            throw new LinkageError("refused " + name);
                        default:
                            throw new ClassNotFoundException(name);
                    }
                }
                if (!name.equals(P + "Ref")) {
                    return super.loadClass(name, resolve);
                }
                try (InputStream in =
                        ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
                    byte[] b = in.readAllBytes();
                    return defineClass(name, b, 0, b.length);
                } catch (IOException e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        for (String kind : new String[] {"ise", "error", "linkage", "cnfe"}) {
            Class<?> ref = new Throwing(kind).loadClass(P + "Ref");
            try {
                ref.getMethod("make").invoke(null);
                System.out.println(kind + "=ok");
            } catch (java.lang.reflect.InvocationTargetException e) {
                Throwable t = e.getCause();
                System.out.println(kind + "=" + t.getClass().getName() + " cause="
                        + (t.getCause() == null ? "null" : t.getCause().getClass().getName()));
            }
        }
    }

    public static class Target {
    }

    public static class Ref {
        public static Object make() {
            return new Target();
        }
    }
}
