// Interpreter round i1, wave 43, lane L5 -- TIMING probe (not compared) for
// proposal i41-L5 (`opcodes::namespaced_array_answer_admitted`, switch
// `CRATONVM_JIT_CAST_SITE_NAMESPACED_ARRAYS`): a class a user-defined loader
// defined runs `checkcast [Ljava/lang/String;` in a hot loop, the
// `(String[]) list.toArray(new String[0])` shape of a Spring Boot
// `LaunchedClassLoader` class, on 1 and on 8 threads. Before wave 43 each
// execution re-resolved the array through the class-manager write lock.
//
// Run: javac -d out L5W43NamespacedArrayCastBench.java
//      cratonvm --java-home <jdk25> --nojit -cp out L5W43NamespacedArrayCastBench
// Compare the `ns/op` rows with the switch off (`=0`) and on; the answer
// line `sum=` must be the same in both.

import java.io.InputStream;

public class L5W43NamespacedArrayCastBench {
    static final String WORKER = "L5W43NamespacedArrayCastBench$Worker";

    public static class Worker {
        public static long spin(Object strings, int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                String[] a = (String[]) strings;
                s += a.length;
            }
            return s;
        }
    }

    static final class Child extends ClassLoader {
        Child() {
            super("child", L5W43NamespacedArrayCastBench.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (name.equals(WORKER)) {
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

    public static void main(String[] args) throws Exception {
        java.lang.reflect.Method spin = new Child().loadClass(WORKER).getMethod("spin", Object.class, int.class);
        Object strings = new String[3];
        int n = 2_000_000;
        spin.invoke(null, strings, n / 10);
        for (int threads : new int[] {1, 8}) {
            long[] sums = new long[threads];
            Thread[] ts = new Thread[threads];
            long t0 = System.nanoTime();
            for (int t = 0; t < threads; t++) {
                final int k = t;
                ts[t] = new Thread(() -> {
                    try {
                        sums[k] = (Long) spin.invoke(null, strings, n);
                    } catch (Exception e) {
                        throw new RuntimeException(e);
                    }
                });
                ts[t].start();
            }
            for (Thread t : ts) t.join();
            long dt = System.nanoTime() - t0;
            long sum = 0;
            for (long s : sums) sum += s;
            System.out.println("threads=" + threads + " sum=" + sum + " ns/op=" + (dt / (double) n));
        }
    }
}
