// Interpreter round i1, wave 38, lane L5 -- JVMS §5.3.5 / HotSpot
// `SystemDictionary::resolve_instance_class_or_null`: a VM-initiated load of
// a name through a loader that is NOT parallel-capable, made while the same
// thread is already loading that name through that loader (the loader's
// `loadClass` ran code that resolved it again), is a `ClassCircularityError`.
// The error is recorded against the constant-pool entry (a `LinkageError`),
// so the OUTER resolution of the same entry, which the loader then answers,
// throws it too (JVMS §5.4.3).
//
// `Ref.outer()` does `new X()`; the loader's `loadClass("$X")` calls
// `Ref.inner()`, whose `X.class` names the same `CONSTANT_Class` entry.
//
// Run (no setup):
//   javac -d out L5W38ReentrantLoadCircularity.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W38ReentrantLoadCircularity
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] LOADER-REENTRY #1 ClassCircularityError: L5W38ReentrantLoadCircularity$X ...`
// and the exit line `[ACCESS-DBG] LOADER census: ... reentry-circularities=1 ...`.
//
// Before wave 38 (from the code): the inner resolution met the per-thread
// in-flight guard and fell back to the global store, which defined `$X` in the
// application loader: `plain inner=AppClassLoader`, `plain outer=L`.
// `--compatible` keeps that by design (AGENTS.md). A PARALLEL-CAPABLE loader
// is not covered (HotSpot re-enters its `loadClass` and both rows read `L`;
// `docs/internal/fixed-bugs/interpreter-L5-a-reentrant-load-through-a-parallel-capable-loader-falls-back-to-the-global-store-FIXED-20261003.md`).
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   plain inner=java.lang.ClassCircularityError: L5W38ReentrantLoadCircularity$X
//   plain outer=java.lang.ClassCircularityError: L5W38ReentrantLoadCircularity$X

import java.io.IOException;
import java.io.InputStream;

public class L5W38ReentrantLoadCircularity {
    static final String P = "L5W38ReentrantLoadCircularity$";

    static final class L extends ClassLoader {
        int depth;
        final String label;

        L(String label) {
            super(L5W38ReentrantLoadCircularity.class.getClassLoader());
            this.label = label;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            Class<?> c = findLoadedClass(name);
            if (c != null) {
                return c;
            }
            if (name.equals(P + "X")) {
                depth++;
                try {
                    if (depth == 1) {
                        try {
                            Class<?> ref = findLoadedClass(P + "Ref");
                            Object r = ref.getMethod("inner").invoke(null);
                            System.out.println(label + " inner=" + r);
                        } catch (java.lang.reflect.InvocationTargetException e) {
                            System.out.println(label + " inner=" + e.getCause());
                        } catch (Exception e) {
                            System.out.println(label + " inner probe " + e);
                        }
                    }
                } finally {
                    depth--;
                }
                Class<?> again = findLoadedClass(name);
                if (again != null) {
                    return again;
                }
                return def(name);
            }
            if (!name.equals(P + "Ref")) {
                return super.loadClass(name, resolve);
            }
            return def(name);
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

    public static void main(String[] args) throws Exception {
        L l = new L("plain");
        Class<?> ref = l.loadClass(P + "Ref");
        try {
            Object r = ref.getMethod("outer").invoke(null);
            System.out.println("plain outer=" + r);
        } catch (java.lang.reflect.InvocationTargetException e) {
            System.out.println("plain outer=" + e.getCause());
        }
    }

    public static class X {
    }

    public static class Ref {
        public static Object outer() {
            return new X().getClass().getClassLoader().getClass().getSimpleName();
        }

        public static Object inner() {
            return X.class.getClassLoader().getClass().getSimpleName();
        }
    }
}
