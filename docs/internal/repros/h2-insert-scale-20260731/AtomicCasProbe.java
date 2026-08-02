import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Single-threaded AtomicLong/AtomicInteger CAS fidelity probe.
 *
 * With one thread and no contention, `compareAndSet(current, next)` must
 * ALWAYS succeed on the first try. Any non-zero `retries` here is a VM defect,
 * and it is the exact shape that would break org.h2.util.MemoryEstimator:
 * its CAS-failure path discards the freshly computed skip counter and reuses
 * the OLD one (`statsData & (INIT_BIT|SKIP_SUM_MASK|COUNTER_MASK)`), so a
 * spurious failure raises the sampling percentage while leaving the running
 * average about right — precisely TestMemoryEstimator's `pct=8` failure.
 *
 * Usage: AtomicCasProbe [iterations]
 */
public class AtomicCasProbe {

    static long lretries;
    static long iretries;
    static long lmismatch;

    static long stepLong(AtomicLong a, long add) {
        long cur = a.get();
        long next = cur + add;
        while (!a.compareAndSet(cur, next)) {
            lretries++;
            cur = a.get();
            next = cur + add;
        }
        return next;
    }

    static int stepInt(AtomicInteger a, int add) {
        int cur = a.get();
        int next = cur + add;
        while (!a.compareAndSet(cur, next)) {
            iretries++;
            cur = a.get();
            next = cur + add;
        }
        return next;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        AtomicLong al = new AtomicLong();
        AtomicInteger ai = new AtomicInteger();
        long expectL = 0;
        int expectI = 0;
        for (int i = 0; i < n; i++) {
            // Values that exercise the high 32 bits and sign bit, the way
            // MemoryEstimator's bit-packed stats word does.
            long add = ((long) (i & 0xFF) << 32) | (i & 0xFFFF);
            expectL += add;
            long got = stepLong(al, add);
            if (got != expectL) {
                lmismatch++;
                if (lmismatch == 1) {
                    System.out.println("first long mismatch at i=" + i + " got=" + got + " want=" + expectL);
                }
                expectL = got;
            }
            expectI += (i & 0x7F) - 63;
            stepInt(ai, (i & 0x7F) - 63);
        }
        System.out.println("iterations=" + n);
        System.out.println("longRetries=" + lretries);
        System.out.println("intRetries=" + iretries);
        System.out.println("longMismatches=" + lmismatch);
        System.out.println("longValueOk=" + (al.get() == expectL) + " value=" + al.get());
        System.out.println("intValueOk=" + (ai.get() == expectI) + " value=" + ai.get());
        System.out.println(lretries == 0 && iretries == 0 && lmismatch == 0 && al.get() == expectL
                && ai.get() == expectI ? "PASS" : "FAIL");
    }
}
