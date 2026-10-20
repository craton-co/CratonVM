import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

/**
 * gcf f4/r (2026-10-01): a POSITIVE CONTROL for the helper-window rows of
 * gce-e2t-proposal-helper-window-pass-skips-peers-without-compiled-frames.
 * vf1 r-5 judged the miss check vacuous: no OFF arm printed a single
 * {@code helper-window tid=} line, so "no miss" said nothing.
 *
 * <p>Here {@code threads} (default 6) workers spend their life BLOCKED from
 * inside a hot, compiled method: {@code spin} is warmed until the JIT has
 * compiled it, and it parks ({@code LockSupport.parkNanos}) and sleeps
 * ({@code Thread.sleep}) between allocations, so at almost any pause a
 * blocked peer has compiled frames on its native stack -- exactly what the
 * helper-window pass exists for. Main allocates garbage for {@code seconds}
 * (default 6) to force collections, then stops the workers.
 *
 * <p>Stdout (deterministic): {@code workers=<n> checksum-ok PROBE-OK}. Judge
 * stderr under {@code CRATONVM_DBG_XT_JIT_ROOT_SCAN=1}: the OFF arm must print
 * at least one {@code helper-window tid=} line (else this control did not
 * fire and the row decides nothing); a line with {@code depth=Some(0)} or
 * {@code depth=None} on the OFF arm is a window the ON arm would have skipped.
 */
public class GcfHelperWindowControlProbe {
    static volatile boolean stop;
    static volatile Object sink;

    static long spin(long seed, int park) throws InterruptedException {
        long acc = seed;
        for (int i = 0; i < 64; i++) {
            Object[] box = new Object[] {Long.valueOf(acc + i)};
            acc = acc * 31 + ((Long) box[0]).longValue();
            if (park > 0 && (i & 15) == 0) {
                if ((i & 16) == 0) {
                    LockSupport.parkNanos(200_000);
                } else {
                    Thread.sleep(0, 200_000);
                }
            }
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        int seconds = args.length > 1 ? Integer.parseInt(args[1]) : 6;
        // Warm `spin` without parking so the JIT compiles it before the workers block in it.
        long warm = 0;
        for (int i = 0; i < 200_000; i++) {
            warm += spin(i, 0);
        }
        AtomicLong done = new AtomicLong();
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long acc = id;
                try {
                    while (!stop) {
                        acc ^= spin(acc, 1);
                    }
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
                done.incrementAndGet();
                sink = acc;
            }, "hw-" + t);
            ts[t].start();
        }
        long end = System.nanoTime() + seconds * 1_000_000_000L;
        while (System.nanoTime() < end) {
            for (int i = 0; i < 10_000; i++) {
                sink = new byte[256];
            }
        }
        stop = true;
        for (Thread t : ts) {
            t.join();
        }
        System.err.println("warm=" + warm);
        System.out.println("workers=" + done.get() + (done.get() == threads ? " checksum-ok PROBE-OK" : " PROBE-FAIL"));
    }
}
