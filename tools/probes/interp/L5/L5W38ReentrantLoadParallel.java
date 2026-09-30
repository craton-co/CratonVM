// Interpreter round i1, wave 38, lane L5 -- the PARALLEL-CAPABLE twin of
// `L5W38ReentrantLoadCircularity`: a VM-initiated load of `$X` through a
// parallel-capable loader, made while the same thread is already loading `$X`
// through it. HotSpot takes no placeholder lock for such a loader and simply
// calls its `loadClass` again: the inner call defines `$X`, and the outer one
// finds it with `findLoadedClass`. Both rows name the loader.
//
// Evidence for `docs/internal/fixed-bugs/interpreter-L5-a-reentrant-load-through-a-parallel-capable-loader-falls-back-to-the-global-store-FIXED-20261003.md`
// (fixed in wave 39 under `--jdk-only`: the re-entry asks the loader again;
// see also `L5W39ReentrantParallelReask`). Before wave 39, and still under
// `--compatible` by design, the inner resolution meets the per-thread
// in-flight guard of `drive_defining_loader_load_named` (same referencing
// class, same name) and falls back to the global store, which defines `$X` in
// the application loader: `par inner=AppClassLoader`, `par outer=L`.
//
// Run (no setup):
//   javac -d out L5W38ReentrantLoadParallel.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W38ReentrantLoadParallel
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   par inner=L
//   par outer=L

import java.io.IOException;
import java.io.InputStream;

public class L5W38ReentrantLoadParallel {
    static final String P = "L5W38ReentrantLoadParallel$";

    static final class L extends ClassLoader {
        static {
            registerAsParallelCapable();
        }

        int depth;

        L() {
            super(L5W38ReentrantLoadParallel.class.getClassLoader());
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
                            System.out.println("par inner=" + r);
                        } catch (java.lang.reflect.InvocationTargetException e) {
                            System.out.println("par inner=" + e.getCause());
                        } catch (Exception e) {
                            System.out.println("par inner probe " + e);
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
        L l = new L();
        Class<?> ref = l.loadClass(P + "Ref");
        try {
            Object r = ref.getMethod("outer").invoke(null);
            System.out.println("par outer=" + r);
        } catch (java.lang.reflect.InvocationTargetException e) {
            System.out.println("par outer=" + e.getCause());
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
