// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 92, lane L3 (`CRATONVM_BOX_WRAPPER_CID_PER_VM`):
// what does a native box cost, on one thread and on several?
//
// `java.lang.reflect.Array.get` is a JDK `native` in both modes, and its
// CratonVM native boxes a primitive element through
// `lang_math::alloc_wrapper`, which -- with the switch unset -- takes the
// PROCESS-GLOBAL `WRAPPER_CIDS` mutex and a hash probe per box to find the
// wrapper's ClassId. With the switch on it is one atomic load from the VM's
// reflect-door caches. The element values are outside every box cache
// (HotSpot's `Reflection::box` allocates a fresh box anyway), so every call
// allocates.
//
//   st-int / st-long / st-double / st-char   one thread, Array.get boxing
//   mt4-int                                  four threads at once, Array.get
//                                            boxing an int (wall ns per op,
//                                            all threads' ops counted)
//   st-int-control                           one thread, the same loop over
//                                            an Integer[] (no boxing native)
//
// The mt row is where the process lock shows; the st rows give its
// uncontended cost; the control must not move.
//
// Output: "row <name> min <ns> med <ns> ns/op" and "checksum <name> <n>",
// then "fresh-box array-get-int <bool>".
// Run: javac -d . L3W92BoxBench.java
//      java -cp . L3W92BoxBench                                             (HotSpot)
//      cratonvm --jdk-only -cp . L3W92BoxBench
//      CRATONVM_BOX_WRAPPER_CID_PER_VM=1 cratonvm --jdk-only -cp . L3W92BoxBench
//      (and the same pair with --compatible and with --nojit; interleave 5x)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum st-int 1081606144
// checksum st-long 1081606144
// checksum st-double 1082130432
// checksum st-char 1081606144
// checksum mt4-int 4326424576
// checksum st-int-control 1081606144
// fresh-box array-get-int true
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Array;
import java.util.Arrays;
import java.util.Locale;

public class L3W92BoxBench {
    static final int OPS = 1 << 20;
    static final int REPS = 7;

    static final int[] INTS = new int[64];
    static final long[] LONGS = new long[64];
    static final double[] DOUBLES = new double[64];
    static final char[] CHARS = new char[64];
    static final Integer[] BOXED = new Integer[64];

    static {
        for (int k = 0; k < 64; k++) {
            INTS[k] = 1000 + k;
            LONGS[k] = 1000 + k;
            DOUBLES[k] = 1000.5 + k;
            CHARS[k] = (char) (1000 + k);
            BOXED[k] = 1000 + k;
        }
    }

    static long loop(String kind) {
        long sum = 0;
        switch (kind) {
            case "int":
                for (int i = 0; i < OPS; i++) {
                    sum += ((Integer) Array.get(INTS, i & 63)).intValue();
                }
                return sum;
            case "long":
                for (int i = 0; i < OPS; i++) {
                    sum += ((Long) Array.get(LONGS, i & 63)).longValue();
                }
                return sum;
            case "double": {
                double d = 0;
                for (int i = 0; i < OPS; i++) {
                    d += ((Double) Array.get(DOUBLES, i & 63)).doubleValue();
                }
                return (long) d;
            }
            case "char":
                for (int i = 0; i < OPS; i++) {
                    sum += ((Character) Array.get(CHARS, i & 63)).charValue();
                }
                return sum;
            default:
                for (int i = 0; i < OPS; i++) {
                    sum += ((Integer) Array.get(BOXED, i & 63)).intValue();
                }
                return sum;
        }
    }

    static void print(String name, long[] t, long checksum) {
        long[] c = t.clone();
        Arrays.sort(c);
        double med = c.length % 2 == 1 ? c[c.length / 2] : (c[c.length / 2 - 1] + c[c.length / 2]) / 2.0;
        System.out.printf(Locale.ROOT, "row %s min %.2f med %.2f ns/op%n", name,
                c[0] / (double) OPS, med / OPS);
        System.out.println("checksum " + name + " " + checksum);
    }

    static void single(String name, String kind, boolean print) {
        long[] t = new long[REPS];
        long checksum = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            checksum = loop(kind);
            t[r] = System.nanoTime() - t0;
        }
        if (print) {
            print(name, t, checksum);
        }
    }

    static void multi(String name, int threads, boolean print) throws InterruptedException {
        long[] t = new long[REPS];
        long checksum = 0;
        for (int r = 0; r < REPS; r++) {
            long[] sums = new long[threads];
            Thread[] ts = new Thread[threads];
            java.util.concurrent.CountDownLatch go = new java.util.concurrent.CountDownLatch(1);
            for (int k = 0; k < threads; k++) {
                final int slot = k;
                ts[k] = new Thread(() -> {
                    try {
                        go.await();
                    } catch (InterruptedException e) {
                        throw new RuntimeException(e);
                    }
                    sums[slot] = loop("int");
                });
                ts[k].start();
            }
            long t0 = System.nanoTime();
            go.countDown();
            for (Thread th : ts) {
                th.join();
            }
            // Wall time over all threads' ops: ns per op as the process sees it.
            t[r] = (System.nanoTime() - t0) / threads;
            checksum = 0;
            for (long s : sums) {
                checksum += s;
            }
        }
        if (print) {
            print(name, t, checksum);
        }
    }

    public static void main(String[] args) throws Throwable {
        // Warm-up: every loop past the JIT thresholds once.
        for (String kind : new String[] {"int", "long", "double", "char", "control"}) {
            single("warm", kind, false);
        }
        multi("warm", 4, false);

        single("st-int", "int", true);
        single("st-long", "long", true);
        single("st-double", "double", true);
        single("st-char", "char", true);
        multi("mt4-int", 4, true);
        single("st-int-control", "control", true);

        // HotSpot's JVM_GetArrayElement boxes through Reflection::box, which
        // allocates (java_lang_boxing_object::create): no Integer cache, so two
        // reads of the same small element are two objects.
        int[] five = {5};
        System.out.println("fresh-box array-get-int " + (Array.get(five, 0) != Array.get(five, 0)));
    }
}
