/*
 * Interpreter round i1, wave 23, lane L7: the cost of the per-frame
 * structured-locking record (`Frame::held_monitors`,
 * `interpreter::held_monitors`, JVMS §2.11.10).
 *
 * The record adds, per `monitorenter`, a push onto an inline two-slot
 * `SmallVec`; per `monitorexit`, a compare against the newest entry and a pop;
 * per return and per frame pop, one `is_empty` test. Nothing else is on a hot
 * path. This bench times the three shapes that pay for it:
 *
 *   syncBlock    an uncontended `synchronized` block in a loop (enter + exit)
 *   syncCall     a call to a method whose body is a `synchronized` block
 *                (enter + exit + push + pop + the return test)
 *   plainCall    a call to an empty-ish method with no monitor (the return
 *                and frame-pop tests only; must not move)
 *   nested2      two nested blocks (both entries inline)
 *
 * Run: CratonVM `--nojit` (the interpreter; with the JIT the loops compile),
 * default mode. A/B against the previous build, interleaved; timings (ns per
 * iteration, median of 5 rounds) go to stderr. Expected direction:
 * `plainCall` unchanged (noise), `syncBlock` / `syncCall` / `nested2` within a
 * few percent (the lock itself is a CAS each way; the record is a store and a
 * compare).
 *
 * stdout is deterministic and identical on HotSpot 25 (`-Xint` or not):
 *
 *   checksum syncBlock=200000 syncCall=200000 plainCall=200000 nested2=400000
 *   held=false
 */
public class L7W23SyncBlockBench {
    static final int N = 200_000;
    static final Object LOCK = new Object();
    static final Object LOCK2 = new Object();
    static int field;

    static int syncBlock() {
        int c = 0;
        for (int i = 0; i < N; i++) {
            synchronized (LOCK) {
                c++;
            }
        }
        return c;
    }

    static int lockedIncrement(int c) {
        synchronized (LOCK) {
            return c + 1;
        }
    }

    static int syncCall() {
        int c = 0;
        for (int i = 0; i < N; i++) {
            c = lockedIncrement(c);
        }
        return c;
    }

    static int plainIncrement(int c) {
        return c + 1;
    }

    static int plainCall() {
        int c = 0;
        for (int i = 0; i < N; i++) {
            c = plainIncrement(c);
        }
        return c;
    }

    static int nested2() {
        int c = 0;
        for (int i = 0; i < N; i++) {
            synchronized (LOCK) {
                synchronized (LOCK2) {
                    c += 2;
                }
            }
        }
        return c;
    }

    interface Row {
        int run();
    }

    static int time(String name, Row row) {
        long[] ns = new long[5];
        int result = 0;
        for (int round = 0; round < 5; round++) {
            long t0 = System.nanoTime();
            result = row.run();
            ns[round] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(ns);
        System.err.printf(java.util.Locale.ROOT, "%-10s %8.1f ns/iter%n", name, (double) ns[2] / N);
        return result;
    }

    public static void main(String[] args) {
        // Warm-up round so class init and first-call resolution are off the clock.
        syncBlock();
        syncCall();
        plainCall();
        nested2();
        int a = time("syncBlock", L7W23SyncBlockBench::syncBlock);
        int b = time("syncCall", L7W23SyncBlockBench::syncCall);
        int c = time("plainCall", L7W23SyncBlockBench::plainCall);
        int d = time("nested2", L7W23SyncBlockBench::nested2);
        System.out.println("checksum syncBlock=" + a + " syncCall=" + b + " plainCall=" + c + " nested2=" + d);
        System.out.println("held=" + (Thread.holdsLock(LOCK) || Thread.holdsLock(LOCK2)));
    }
}
