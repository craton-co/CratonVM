/**
 * The shape RBC.6's `getfield`/`putfield` exclusion actually blocked: javac's
 * `synchronized (…) { … }` cleanup handler, with field accesses inside the
 * protected range.
 *
 * `probes/Rbc6FieldProbe.java` covers the wrong-local half — a handler reading
 * a non-parameter local observes 0 instead of the value the try body stored.
 * This covers the half that is worse than a wrong number: javac's synthetic
 * handler is
 *
 *     astore_N ; aload_MONITOR ; monitorexit ; aload_N ; athrow
 *
 * and `aload_MONITOR` reads a NON-parameter local (javac's `dup; astore` of
 * the monitor expression). If the exceptional frame hands that back as null,
 * the `monitorexit` either throws over the original exception or never runs —
 * and the lock is then held forever by a thread that has left the block. A
 * leaked monitor is invisible in the returned value, so it is checked
 * directly: a second thread must be able to enter the same block afterwards.
 *
 * `sun.util.locale.provider.JRELocaleProviderAdapter.getDateFormatSymbolsProvider`
 * is this exact bytecode.
 *
 * Three shape constraints, each learned by watching the probe fail to prove
 * anything:
 *
 *  * `static` methods. As small INSTANCE methods these were never even
 *    invocation-counted — `CRATONVM_DBG=jit-method-stats` reported three
 *    tracked methods for the entire run — so every assertion was a statement
 *    about interpreted code.
 *  * bodies past the inlining size cap (hence `mix`), for the same reason:
 *    an inlined callee is never compiled as a method of its own.
 *  * the counter lives in a PARAMETER-reached object, not a static field.
 *    `getstatic`/`putstatic` (0xb2/0xb3) are still outside
 *    `precise_frame_publishing_opcode`'s admitted set, so one static-field
 *    access inside the block withholds coverage for the whole method and the
 *    RBC.6 bail comes back — masking the thing under test.
 */
public final class SyncBlockFieldProbe {

    static final class Holder {
        int value = 7;
        /** getfield + putfield inside the protected range. */
        int count;
    }

    /**
     * Throws through a `synchronized` block: the synthetic cleanup handler has
     * to release the monitor on the way out, using its non-parameter local.
     */
    static int throwsThroughSyncBlock(Holder lock, Holder h, int n) {
        int scratch = 0;
        synchronized (lock) {
            scratch = n * 7 + 3;
            scratch = mix(scratch, n);
            lock.count = lock.count + 1;
            return h.value + scratch;
        }
    }

    /** Same block, but the NPE is caught here and a non-parameter local read. */
    static int caughtThroughSyncBlock(Holder lock, Holder h, int n) {
        int scratch = 0;
        try {
            synchronized (lock) {
                scratch = n * 13 + 1;
                scratch = mix(scratch, n);
                lock.count = lock.count + 1;
                return h.value + scratch;
            }
        } catch (NullPointerException e) {
            return scratch;
        }
    }

    /** `getDateFormatSymbolsProvider`'s exact double-checked shape. */
    static Object lazyProvider(Holder lock, Object[] cell, Object candidate) {
        if (cell[0] == null) {
            Object made = candidate;
            synchronized (lock) {
                if (cell[0] == null) {
                    cell[0] = made;
                }
            }
        }
        return cell[0];
    }

    /** Enough straight-line work to keep the callers past the inline cap. */
    private static int mix(int scratch, int n) {
        int s = scratch;
        for (int i = 0; i < 8; i++) {
            s = s * 31 + (n ^ (i * 7 + 1));
        }
        return s;
    }

    /** Does a second thread still get the lock? A leaked monitor hangs here. */
    private static boolean lockIsFree(final Holder lock) throws InterruptedException {
        final boolean[] got = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (lock) {
                got[0] = true;
            }
        });
        t.start();
        t.join(10_000);
        return got[0];
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length == 0 ? 200_000 : Integer.parseInt(args[0]);
        final Holder lock = new Holder();
        Holder live = new Holder();
        Object[] cell = new Object[1];

        long acc = 0;
        int npes = 0;
        int expectedCount = 0;

        for (int i = 0; i < iterations; i++) {
            // Mostly non-throwing so the methods get hot and compiled, then a
            // null receiver on a fraction so the handler runs with the
            // compiled frame live.
            Holder h = (i % 8 == 0) ? null : live;
            try {
                acc += throwsThroughSyncBlock(lock, h, i);
            } catch (NullPointerException e) {
                npes++;
            }
            // `count` is incremented before the throwing getfield, so it
            // advances on EVERY call regardless of which path was taken.
            expectedCount++;

            acc += caughtThroughSyncBlock(lock, h, i);
            expectedCount++;

            acc += lazyProvider(lock, cell, "P") == "P" ? 1 : 0;
        }

        // 1. The lock must not have leaked out of any of the exceptional exits.
        if (!lockIsFree(lock)) {
            throw new AssertionError(
                "monitor leaked: a second thread could not enter the block after "
                    + npes + " exceptional exits");
        }

        // 2. Every call incremented `count` exactly once, throwing or not.
        if (lock.count != expectedCount) {
            throw new AssertionError(
                "count=" + lock.count + " expected=" + expectedCount);
        }

        // 3. Mutual exclusion still holds with the block compiled.
        final int perThread = 20_000;
        final Holder live2 = live;
        Runnable bump = () -> {
            for (int i = 0; i < perThread; i++) {
                caughtThroughSyncBlock(lock, live2, i);
            }
        };
        int before = lock.count;
        Thread a = new Thread(bump);
        Thread b = new Thread(bump);
        a.start();
        b.start();
        a.join(60_000);
        b.join(60_000);
        if (a.isAlive() || b.isAlive()) {
            throw new AssertionError("contended threads did not finish — monitor leaked");
        }
        if (lock.count != before + 2 * perThread) {
            throw new AssertionError(
                "lost update under contention: count=" + lock.count
                    + " expected=" + (before + 2 * perThread));
        }

        System.out.println("acc=" + acc);
        System.out.println("npes=" + npes);
        System.out.println("count=" + lock.count);
        System.out.println("throws(null,5)=" + npeOf(lock, 5));
        System.out.println("caught(null,5)=" + caughtThroughSyncBlock(lock, null, 5)
            + " expect=" + mix(5 * 13 + 1, 5));
        System.out.println("SyncBlockFieldProbe OK");
    }

    private static String npeOf(Holder lock, int n) {
        try {
            throwsThroughSyncBlock(lock, null, n);
            return "NO-THROW";
        } catch (NullPointerException e) {
            return "NPE";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }
}
