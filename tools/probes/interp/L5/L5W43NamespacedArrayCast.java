// Interpreter round i1, wave 43, lane L5 -- `checkcast` / `instanceof` of
// ARRAY types in a class a user-defined loader defined, repeated so the
// per-thread cast-site table serves them
// (docs/internal/fixed-bugs/interpreter-L5-proposal-a-loaders-array-class-answers-are-recorded-FIXED-20261007.md).
// Before wave 43 a loader-namespaced class's array-target site was never
// filled, so every execution re-resolved the array (for `String[]`, through
// the class-manager write lock); wave 43 fills it when the array's element is
// a `java/` class, a primitive, or the loader's own recorded answer
// (`opcodes::namespaced_array_answer_admitted`, switch
// `CRATONVM_JIT_CAST_SITE_NAMESPACED_ARRAYS`). The answers must not change:
// this probe checks that a child-first loader's own `Shared` keeps its arrays
// apart from the application's `Shared[]` at a filled site.
//
// Rows (each `a b c d e` counts, over N executions of one site each:
//   a `String[]` instanceof String[]; b `Object[]` checkcast String[] ->
//   ClassCastException; c the loader's `Worker[]` instanceof Worker[];
//   d the application's `Shared[]` instanceof Shared[]; e the application's
//   `Shared[][]` instanceof Shared[][]):
//   delegating   the loader defines Worker and delegates Shared
//   child-first  the loader defines Worker and its OWN Shared
//   again        the child-first loader's Worker run a second time (every
//                site filled by the first run)
//
// Run (no setup):
//   javac -d out L5W43NamespacedArrayCast.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W43NamespacedArrayCast
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical):
//   delegating=20000 20000 20000 20000 20000
//   child-first=20000 20000 20000 0 0
//   again=20000 20000 20000 0 0
//
// Positive control (the orchestrator): `CRATONVM_DBG_FIELD_SITE=1` prints the
// `[site-cache]` counters at exit; with the switch on, `cast: ... fill=`
// rises by the probe's array sites and `reject_loader=` falls by about
// 5 x 3 x N (it counted every execution of these sites before).

import java.io.InputStream;
import java.lang.reflect.Array;
import java.lang.reflect.Method;

public class L5W43NamespacedArrayCast {
    static final int N = 20000;
    static final String WORKER = "L5W43NamespacedArrayCast$Worker";
    static final String SHARED = "L5W43NamespacedArrayCast$Shared";

    public static class Shared {
    }

    public static class Worker {
        public static Object[] make(int n) {
            return new Worker[n];
        }

        public static String run(Object strings, Object objects, Object workers, Object shared,
                Object shared2d, int n) {
            int a = 0, b = 0, c = 0, d = 0, e = 0;
            for (int i = 0; i < n; i++) {
                if (strings instanceof String[]) a++;
                try {
                    Object x = (String[]) objects;
                    if (x == null) a--;
                } catch (ClassCastException ex) {
                    b++;
                }
                if (workers instanceof Worker[]) c++;
                if (shared instanceof Shared[]) d++;
                if (shared2d instanceof Shared[][]) e++;
            }
            return a + " " + b + " " + c + " " + d + " " + e;
        }
    }

    /** Defines the names in `own` from this probe's bytes; delegates the rest. */
    static final class Child extends ClassLoader {
        final java.util.Set<String> own;

        Child(java.util.Set<String> own) {
            super("child", L5W43NamespacedArrayCast.class.getClassLoader());
            this.own = own;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (own.contains(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        try (InputStream in = getParent().getResourceAsStream(name + ".class")) {
                            byte[] b = in.readAllBytes();
                            c = defineClass(name, b, 0, b.length);
                        } catch (java.io.IOException ioe) {
                            throw new ClassNotFoundException(name, ioe);
                        }
                    }
                    return c;
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    static String row(Child loader) throws Exception {
        Class<?> worker = loader.loadClass(WORKER);
        Object workers = worker.getMethod("make", int.class).invoke(null, 1);
        Method run = worker.getMethod("run", Object.class, Object.class, Object.class, Object.class,
                Object.class, int.class);
        return (String) run.invoke(null, new String[1], new Object[1], workers, new Shared[1],
                new Shared[1][1], N);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("delegating=" + row(new Child(java.util.Set.of(WORKER))));
        Child childFirst = new Child(java.util.Set.of(WORKER, SHARED));
        System.out.println("child-first=" + row(childFirst));
        System.out.println("again=" + row(childFirst));
    }
}
