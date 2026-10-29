import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

// JIT round 21, lane refs: the NEGATIVE control for REFS21-6
// (`CRATONVM_XT_HELPER_WINDOW_PARK_CREDIT`, docs/known-issues/gc/
// jr21-blocked-peer-helper-window-roots-stale-rust-stack-words-FIXED-20261026.md).
//
// Worker threads run warmed, compiled methods that keep a freshly allocated object ONLY in a
// local (register / spill / shadow stack) across a park (LockSupport.parkNanos, a Condition
// await, and a plain park) while a churn thread allocates and a GC thread calls System.gc(). After
// every wake each worker re-checks the object's contents. A credited peer whose compiled-frame
// holder the deposit's chain walk missed would show as bad > 0, a crash, or an all-zero object.
// Every fact equals HotSpot 25 (bad 0). Bounded: ROUNDS x WORKERS parks.
//
// Arms: switch on vs off, x ZGC (default) / -XX:+UseG1GC / -XX:+UseGenerationalGC, x --jdk-only /
// --compatible; also -Xmx64m (more relocation) and CRATONVM_DBG_GC_STRESS=1.
public class R21RefsParkCreditStress {
    static final int WORKERS = 6;
    static final int ROUNDS = 400;
    static volatile boolean stop;
    static volatile Object sink;
    static volatile int bad;
    static final ReentrantLock LOCK = new ReentrantLock();
    static final Condition COND = LOCK.newCondition();

    static final class Box {
        final long a;
        final long b;
        final int[] payload;

        Box(long seed) {
            a = seed;
            b = ~seed;
            payload = new int[16];
            for (int i = 0; i < payload.length; i++) {
                payload[i] = (int) (seed * 31 + i);
            }
        }

        boolean ok(long seed) {
            if (a != seed || b != ~seed || payload.length != 16) {
                return false;
            }
            for (int i = 0; i < payload.length; i++) {
                if (payload[i] != (int) (seed * 31 + i)) {
                    return false;
                }
            }
            return true;
        }
    }

    // The object lives only in this frame across the park.
    static int parkNanosHolding(long seed, boolean park) {
        Box b = new Box(seed);
        if (park) {
            LockSupport.parkNanos(200_000L);
        }
        return b.ok(seed) ? 0 : 1;
    }

    static int awaitHolding(long seed, boolean park) throws InterruptedException {
        Box b = new Box(seed);
        if (park) {
            LOCK.lock();
            try {
                COND.await(200, TimeUnit.MICROSECONDS);
            } finally {
                LOCK.unlock();
            }
        }
        return b.ok(seed) ? 0 : 1;
    }

    static int parkHolding(long seed, boolean park) {
        Box b = new Box(seed);
        Object blocker = new Object();
        if (park) {
            LockSupport.parkNanos(blocker, 150_000L);
        }
        return (b.ok(seed) ? 0 : 1) + (blocker.getClass() == Object.class ? 0 : 1);
    }

    static int work(long seed, boolean park) throws InterruptedException {
        return parkNanosHolding(seed, park) + awaitHolding(seed + 1, park) + parkHolding(seed + 2, park);
    }

    public static void main(String[] args) throws Exception {
        // Warm every kernel (>= 20000 calls) without parking, in the method that drives them.
        int warm = 0;
        for (int i = 0; i < 25_000; i++) {
            warm += work(i, false);
        }
        System.out.println("fact warm-bad " + warm);

        Thread churn = new Thread(() -> {
            while (!stop) {
                Object[] a = new Object[128];
                a[0] = a;
                sink = a;
            }
        });
        churn.setDaemon(true);
        churn.start();
        Thread gc = new Thread(() -> {
            while (!stop) {
                System.gc();
                try {
                    Thread.sleep(3);
                } catch (InterruptedException e) {
                    return;
                }
            }
        });
        gc.setDaemon(true);
        gc.start();

        Thread[] workers = new Thread[WORKERS];
        for (int w = 0; w < WORKERS; w++) {
            final int id = w;
            workers[w] = new Thread(() -> {
                int local = 0;
                for (int r = 0; r < ROUNDS; r++) {
                    try {
                        local += work(id * 1_000_003L + r, true);
                    } catch (InterruptedException e) {
                        return;
                    }
                }
                if (local != 0) {
                    synchronized (R21RefsParkCreditStress.class) {
                        bad += local;
                    }
                }
            });
            workers[w].start();
        }
        for (Thread t : workers) {
            t.join();
        }
        stop = true;
        gc.join();
        System.out.println("fact bad " + bad);
    }
}
