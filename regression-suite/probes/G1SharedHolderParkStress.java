import java.util.concurrent.locks.LockSupport;

// jr21-g1-initiator-movable-claim-unpinned-peer-words: one object held by BOTH an allocating
// thread's compiled frame (the collection's initiator, whose oop map claims it movable) and a
// parked peer's compiled frame (a deposit / helper-window word nobody rewrites). Under the old
// G1 filter the initiator's claim unpinned the peer's word too, so the peer read a relocated
// object after its wake. Every fact equals HotSpot 25 (bad 0).
public class G1SharedHolderParkStress {
    static final int PAIRS = 4;
    static final int ROUNDS = 1500;
    static volatile boolean stop;
    static volatile Object sink;
    static final Object[] SLOT = new Object[PAIRS];
    static final long[] SEED = new long[PAIRS];
    static volatile int bad;

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

    // The initiator: holds the shared box in a local while it allocates (and so collects).
    static int allocHolding(Box x, long seed, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            Object[] a = new Object[64];
            a[0] = x;
            sink = a;
            acc += a.length;
        }
        return (x.ok(seed) ? 0 : 1) + (acc == 64 * n ? 0 : 1);
    }

    // The peer: holds the same box in a local across a park.
    static int parkHolding(Box x, long seed, boolean park) {
        if (park) {
            LockSupport.parkNanos(100_000L);
        }
        return x.ok(seed) ? 0 : 1;
    }

    public static void main(String[] args) throws Exception {
        int warm = 0;
        Box w = new Box(7);
        for (int i = 0; i < 25_000; i++) {
            warm += allocHolding(w, 7, 2) + parkHolding(w, 7, false);
        }
        System.out.println("fact warm-bad " + warm);

        Thread gc = new Thread(() -> {
            while (!stop) {
                System.gc();
                try {
                    Thread.sleep(5);
                } catch (InterruptedException e) {
                    return;
                }
            }
        });
        gc.setDaemon(true);
        gc.start();

        Thread[] ts = new Thread[PAIRS * 2];
        for (int p = 0; p < PAIRS; p++) {
            final int id = p;
            // Allocator: publishes a fresh box, then holds it while allocating.
            ts[2 * p] = new Thread(() -> {
                int local = 0;
                for (int r = 0; r < ROUNDS; r++) {
                    long seed = id * 1_000_003L + r;
                    Box x = new Box(seed);
                    synchronized (SLOT) {
                        SLOT[id] = x;
                        SEED[id] = seed;
                    }
                    local += allocHolding(x, seed, 200);
                }
                if (local != 0) {
                    synchronized (G1SharedHolderParkStress.class) {
                        bad += local;
                    }
                }
            });
            // Parker: picks up the current box and holds it across a park.
            ts[2 * p + 1] = new Thread(() -> {
                int local = 0;
                for (int r = 0; r < ROUNDS; r++) {
                    Box x;
                    long seed;
                    synchronized (SLOT) {
                        x = (Box) SLOT[id];
                        seed = SEED[id];
                    }
                    if (x != null) {
                        local += parkHolding(x, seed, true);
                    } else {
                        Thread.onSpinWait();
                    }
                }
                if (local != 0) {
                    synchronized (G1SharedHolderParkStress.class) {
                        bad += local;
                    }
                }
            });
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        stop = true;
        gc.join();
        System.out.println("fact bad " + bad);
    }
}
