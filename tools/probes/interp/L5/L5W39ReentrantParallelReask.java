// Interpreter round i1, wave 39, lane L5 -- a VM-initiated load through a
// PARALLEL-CAPABLE loader, re-entered from the same referencing class for the
// name the thread is already loading, is asked of the loader again (HotSpot
// takes no placeholder lock for such a loader). Companion of
// `L5W38ReentrantLoadParallel` (one re-entry) with two more shapes:
//
// * `nest`: the loader re-enters twice before it defines `$X` (depth 3); every
//   resolution names the loader.
// * `patho`: the loader re-enters for ever and never defines. HotSpot recurses
//   until `StackOverflowError`, which the loader rethrows up to `main`.
//   CratonVM cuts the nesting at 16 same-site re-entries with the same error
//   (`constants.rs` `drive_defining_loader_load_named`,
//   `PARALLEL_REENTRY_BOUND`).
//
// Fix for `docs/internal/fixed-bugs/interpreter-L5-a-reentrant-load-through-a-parallel-capable-loader-falls-back-to-the-global-store-FIXED-20261003.md`.
// Before wave 39 (every mode) the second request met the per-thread in-flight
// guard and fell back to the global store, which defined `$X` in the
// application loader: `nest inner1=AppClassLoader` (and no `inner2` row)
// and `patho=ok AppClassLoader`.
//
// Positive control: `CRATONVM_DBG=access` prints one
// `[ACCESS-DBG] LOADER-REENTRY-PARALLEL #n re-ask: ...` line per re-entry
// (2 for `nest`, 15 for `patho`, then one `... StackOverflowError: ...`), and
// the exit line `[ACCESS-DBG] LOADER census: ... reentry-parallel-reasks=18 ...`
// (`loader-throws-propagated=16` too: each `patho` level's rethrown error is
// its loader's throwable, which HotSpot propagates the same way).
//
// `--compatible`: unchanged by design (the old global fallback):
//   nest inner1=AppClassLoader
//   nest outer=L
//   patho=ok AppClassLoader
//
// Run (no setup):
//   javac -d out L5W39ReentrantParallelReask.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W39ReentrantParallelReask
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   nest inner2=L
//   nest inner1=L
//   nest outer=L
//   patho=java.lang.StackOverflowError

import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;

public class L5W39ReentrantParallelReask {
    static final String P = "L5W39ReentrantParallelReask$";

    static final class L extends ClassLoader {
        static {
            registerAsParallelCapable();
        }

        final int defineAt; // 0 = never define `$X`
        int depth;

        L(int defineAt) {
            super(L5W39ReentrantParallelReask.class.getClassLoader());
            this.defineAt = defineAt;
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
                    if (defineAt == 0 || depth < defineAt) {
                        Object r;
                        try {
                            Class<?> ref = findLoadedClass(P + "Ref");
                            r = ref.getMethod("inner").invoke(null);
                        } catch (InvocationTargetException e) {
                            if (e.getCause() instanceof Error err) {
                                throw err;
                            }
                            r = e.getCause();
                        } catch (ReflectiveOperationException e) {
                            r = "probe " + e;
                        }
                        if (defineAt != 0) {
                            System.out.println("nest inner" + depth + "=" + r);
                        }
                    }
                } finally {
                    depth--;
                }
                Class<?> again = findLoadedClass(name);
                if (again != null) {
                    return again;
                }
                if (defineAt == 0) {
                    throw new ClassNotFoundException(name);
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
        Class<?> ref = new L(3).loadClass(P + "Ref");
        try {
            System.out.println("nest outer=" + ref.getMethod("outer").invoke(null));
        } catch (InvocationTargetException e) {
            System.out.println("nest outer=" + e.getCause());
        }

        Class<?> patho = new L(0).loadClass(P + "Ref");
        try {
            System.out.println("patho=ok " + patho.getMethod("outer").invoke(null));
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            System.out.println("patho=" + (t == null ? "null" : t.getClass().getName()));
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
