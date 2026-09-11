import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import jdk.internal.misc.Unsafe;

/**
 * L5 -- is an `Unsafe` read-modify-write on an ARRAY ELEMENT atomic?
 *
 * Reached the only way anyone can, and BOTH VMs get the same flag on `javac`
 * as well as on `java`:
 *
 *   --add-exports java.base/jdk.internal.misc=ALL-UNNAMED
 *
 * Without it this file does not compile, and a harness that diffs two runs
 * anyway compares an empty file with an empty file and reports zero
 * differences. Check the `rows` trailer (7) before believing a clean diff.
 *
 * # Why an array element rather than a field
 *
 * `unsafe_natives_ext.rs` splits every `getAndSet*` / `getAndAdd*` on the
 * receiver's SHAPE: a field goes through a `compare_and_swap_field` retry
 * loop, an array element went through a bare `get_array_element` +
 * `set_array_element` pair. So the atomicity of one method depended on what
 * you called it on, and no field-shaped probe could see it. Fixed 2026-09-10;
 * this is the probe that scores the fix.
 *
 * # Why the claim COUNT and not the final contents
 *
 * A lost update leaves the array in exactly the state a correct run leaves it
 * in -- all slots null, all counters equal to their final value. What differs
 * is how many threads were TOLD they were the one who took the slot. That is
 * the number `ForkJoinPool$WorkQueue` acts on: it runs the task it was handed.
 * So every row here counts successful claims, never contents.
 *
 * Nothing printed depends on scheduling: with an atomic primitive, exactly one
 * claim per slot happens however the threads interleave.
 */
public class L5CasRace {
    static final Unsafe U = Unsafe.getUnsafe();
    static final int THREADS = 4;
    static final int SLOTS = 2000;
    static final int SPINS = 20000;

    static void par(Runnable body) throws Exception {
        CountDownLatch go = new CountDownLatch(1);
        Thread[] ts = new Thread[THREADS];
        for (int i = 0; i < THREADS; i++) {
            ts[i] = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return;
                }
                body.run();
            });
            ts[i].start();
        }
        go.countDown();
        for (Thread t : ts) {
            t.join();
        }
    }

    public static void main(String[] args) throws Exception {
        long objBase = U.arrayBaseOffset(Object[].class);
        long objScale = U.arrayIndexScale(Object[].class);
        long intBase = U.arrayBaseOffset(int[].class);
        long intScale = U.arrayIndexScale(int[].class);
        long longBase = U.arrayBaseOffset(long[].class);
        long longScale = U.arrayIndexScale(long[].class);
        // The JDK's own `ASHIFT` derivation asserts this; say so rather than
        // printing the values, which are a VM's own choice.
        System.out.println("scalesArePowersOfTwo="
                + (pow2(objScale) && pow2(intScale) && pow2(longScale)));

        // 1. getAndSetReference on a slot: exactly one thread may see non-null.
        final Object[] a1 = new Object[SLOTS];
        final Object tok = new Object();
        java.util.Arrays.fill(a1, tok);
        final AtomicInteger claims1 = new AtomicInteger();
        par(() -> {
            for (int i = 0; i < SLOTS; i++) {
                if (U.getAndSetReference(a1, objBase + i * objScale, null) != null) {
                    claims1.incrementAndGet();
                }
            }
        });
        System.out.println("getAndSetReference claims=" + claims1.get() + "/" + SLOTS);

        // 2. compareAndSetReference on a slot: the control. This arm was
        //    always a CAS, and it is here so a red row above cannot be read as
        //    "this VM has no atomics".
        final Object[] a2 = new Object[SLOTS];
        java.util.Arrays.fill(a2, tok);
        final AtomicInteger claims2 = new AtomicInteger();
        par(() -> {
            for (int i = 0; i < SLOTS; i++) {
                if (U.compareAndSetReference(a2, objBase + i * objScale, tok, null)) {
                    claims2.incrementAndGet();
                }
            }
        });
        System.out.println("compareAndSetReference claims=" + claims2.get() + "/" + SLOTS);

        // 3. getAndSetInt on a slot: one thread per (slot, generation).
        final int[] a3 = new int[SLOTS];
        java.util.Arrays.fill(a3, 7);
        final AtomicInteger claims3 = new AtomicInteger();
        par(() -> {
            for (int i = 0; i < SLOTS; i++) {
                if (U.getAndSetInt(a3, intBase + i * intScale, 0) == 7) {
                    claims3.incrementAndGet();
                }
            }
        });
        System.out.println("getAndSetInt claims=" + claims3.get() + "/" + SLOTS);

        // 4. getAndSetLong on a slot.
        final long[] a4 = new long[SLOTS];
        java.util.Arrays.fill(a4, 7L);
        final AtomicInteger claims4 = new AtomicInteger();
        par(() -> {
            for (int i = 0; i < SLOTS; i++) {
                if (U.getAndSetLong(a4, longBase + i * longScale, 0L) == 7L) {
                    claims4.incrementAndGet();
                }
            }
        });
        System.out.println("getAndSetLong claims=" + claims4.get() + "/" + SLOTS);

        // 5. getAndAddInt on ONE slot: a lost increment is a low total.
        final int[] a5 = new int[1];
        par(() -> {
            for (int i = 0; i < SPINS; i++) {
                U.getAndAddInt(a5, intBase, 1);
            }
        });
        System.out.println("getAndAddInt total=" + a5[0] + "/" + (THREADS * SPINS));

        // 6. getAndAddLong on ONE slot.
        final long[] a6 = new long[1];
        par(() -> {
            for (int i = 0; i < SPINS; i++) {
                U.getAndAddLong(a6, longBase, 1L);
            }
        });
        System.out.println("getAndAddLong total=" + a6[0] + "/" + (long) THREADS * SPINS);

        // 7. compareAndSetInt spin loop on one slot: the second control.
        final int[] a7 = new int[1];
        par(() -> {
            for (int i = 0; i < SPINS; i++) {
                int c;
                do {
                    c = U.getIntVolatile(a7, intBase);
                } while (!U.compareAndSetInt(a7, intBase, c, c + 1));
            }
        });
        System.out.println("compareAndSetInt total=" + a7[0] + "/" + (THREADS * SPINS));

        System.out.println("rows 8");
        System.out.println("DONE L5CasRace");
    }

    static boolean pow2(long v) {
        return v > 0 && (v & (v - 1)) == 0;
    }
}
