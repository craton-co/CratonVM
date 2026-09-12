import java.util.Arrays;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Is `java.util.concurrent` being excluded from virtual tier-up by a prefix
 * test meant for `java.util` COLLECTIONS?
 *
 * `execute_invokevirtual_cached` suppresses invocation-count tier-up when the
 * RECEIVER's class name `starts_with("java/util/")`. The comment says the
 * hazard is a hot java.util graph Spring reaches while building annotation and
 * conversion metadata — collections. But the prefix also matches
 * `java/util/concurrent/locks/ReentrantLock`,
 * `java/util/concurrent/locks/AbstractQueuedSynchronizer`,
 * `java/util/concurrent/ThreadPoolExecutor` … i.e. the whole concurrency stack.
 *
 * The gate reads the RECEIVER's class, not the call site's symbolic owner. So a
 * user-defined subclass of ReentrantLock has a receiver class outside
 * `java/util/` while running byte-for-byte the same inherited `lock()`/
 * `unlock()` bodies. If the subclass is dramatically faster, the prefix is the
 * blocker — no VM rebuild needed to find out.
 *
 * Prints ns/op for both. Same work, same bytecode, different receiver class.
 *
 * ## What the 2026-09-11 rebuild added, and why
 *
 * The original (2026-08-03) printed ONE number per arm from two timed loops.
 * That is what `aqs-thread-handoff-latency-RETIRED-20260805.md` item 3 read
 * 0.77x off, and `composition-native-callback-and-the-promotion-question-20260902.md`
 * then measured the SAME probe scattering ±20 % across reps — a spread wider
 * than the effect it was being asked to resolve, so a single pair of numbers
 * cannot separate the arms at all.
 *
 * So the arms are now INTERLEAVED and repeated, and the probe prints every rep
 * plus the MEDIAN ratio. A median over interleaved reps is immune to the
 * host-load drift that a single A-then-B pair reports as a result. Rep count
 * and loop size are `-Dprobe.reps=` / `-Dprobe.rounds=`; the defaults
 * reproduce the original's work per rep.
 */
public class JavaUtilTierUpExclusionProbe {

    /** Identical behaviour; the only difference is the receiver's class name. */
    private static final class MyLock extends ReentrantLock {
        private static final long serialVersionUID = 1L;
    }

    private static final int WARMUP = Integer.getInteger("probe.warmup", 200_000);
    private static final int ROUNDS = Integer.getInteger("probe.rounds", 2_000_000);
    private static final int REPS = Integer.getInteger("probe.reps", 6);

    // TWO methods, deliberately. A single `lockUnlock(ReentrantLock, int)`
    // called with both receivers turns its `lock.lock()` site POLYMORPHIC, and
    // a poly site costs ~6x a monomorphic one here — which is what the first
    // cut of this probe actually measured (the subclass came out 2.5x SLOWER,
    // an artifact of the shared call site, not of the receiver's class name).
    // Each loop below sees exactly one receiver class.

    private static long lockUnlockPlain(ReentrantLock lock, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            lock.lock();
            lock.unlock();
        }
        return System.nanoTime() - t0;
    }

    private static long lockUnlockSub(MyLock lock, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            lock.lock();
            lock.unlock();
        }
        return System.nanoTime() - t0;
    }

    private static double median(double[] xs) {
        double[] c = xs.clone();
        Arrays.sort(c);
        int n = c.length;
        return (n % 2 == 1) ? c[n / 2] : (c[n / 2 - 1] + c[n / 2]) / 2.0;
    }

    public static void main(String[] args) {
        ReentrantLock plain = new ReentrantLock();
        MyLock sub = new MyLock();

        // Warm both, in both orders, before either is measured.
        lockUnlockPlain(plain, WARMUP);
        lockUnlockSub(sub, WARMUP);
        lockUnlockPlain(plain, WARMUP);
        lockUnlockSub(sub, WARMUP);

        double[] plainNs = new double[REPS];
        double[] subNs = new double[REPS];
        double[] ratio = new double[REPS];

        for (int r = 0; r < REPS; r++) {
            // Interleave, and alternate which arm goes first, so a monotonic
            // host-load drift cannot land entirely on one arm.
            long p, s;
            if ((r & 1) == 0) {
                p = lockUnlockPlain(plain, ROUNDS);
                s = lockUnlockSub(sub, ROUNDS);
            } else {
                s = lockUnlockSub(sub, ROUNDS);
                p = lockUnlockPlain(plain, ROUNDS);
            }
            plainNs[r] = p / (double) ROUNDS;
            subNs[r] = s / (double) ROUNDS;
            ratio[r] = plainNs[r] / subNs[r];
            System.out.printf(
                    "@@JUTIER rep=%d base=%9.1f sub=%9.1f sub_over_base=%.3f%n",
                    r, plainNs[r], subNs[r], ratio[r]);
        }

        System.out.printf("ReentrantLock       (receiver java/util/...)  %9.1f ns/op  [%.1f..%.1f]%n",
                median(plainNs), min(plainNs), max(plainNs));
        System.out.printf("MyLock extends it   (receiver NOT java/util)  %9.1f ns/op  [%.1f..%.1f]%n",
                median(subNs), min(subNs), max(subNs));
        System.out.printf("@@JUTIER median sub_over_base %.3f  [%.3f..%.3f] over %d reps%n",
                median(ratio), min(ratio), max(ratio), REPS);
    }

    private static double min(double[] xs) {
        double m = Double.MAX_VALUE;
        for (double x : xs) m = Math.min(m, x);
        return m;
    }

    private static double max(double[] xs) {
        double m = -Double.MAX_VALUE;
        for (double x : xs) m = Math.max(m, x);
        return m;
    }
}
