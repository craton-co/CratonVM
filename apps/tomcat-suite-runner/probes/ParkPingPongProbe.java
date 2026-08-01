import java.util.concurrent.Semaphore;
import java.util.concurrent.locks.LockSupport;

/**
 * Thread wake-up latency, which is what known-issue tomcat/32.3's remaining
 * assertion (SEQ2) is actually made of.
 *
 * `CRATONVM_DBG_AIO_LATENCY` shows the AIO worker->dispatcher handoff costs
 * only ~55 us, so SEQ2's ~1.3 ms gap is time spent waiting for the peer to
 * send. In that test the "peer" is the embedded server in the SAME VM, and it
 * is blocked in `Semaphore.acquire` waiting for its previous async write to
 * complete before sending the next message. So SEQ2 is gated on how fast one
 * thread can wake another.
 *
 * A: Semaphore ping-pong  -- exactly TesterAsyncTiming's shape
 * B: LockSupport park/unpark -- the primitive underneath it
 * C: Object wait/notify   -- the monitor-based alternative, for contrast
 *
 * Reported as round-trip; halve for one-way wake latency.
 */
public class ParkPingPongProbe {

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2000;

        for (int round = 0; round < 3; round++) {
            System.out.println("--- round " + round + " (iters=" + iters + ") ---");
            semaphoreRoundTrip(iters);
            parkRoundTrip(iters);
            monitorRoundTrip(iters);
        }
    }

    private static void semaphoreRoundTrip(int iters) throws Exception {
        Semaphore a = new Semaphore(0);
        Semaphore b = new Semaphore(0);
        Thread t = new Thread(() -> {
            try {
                for (int i = 0; i < iters; i++) {
                    a.acquire();
                    b.release();
                }
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        });
        t.setDaemon(true);
        t.start();
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            a.release();
            b.acquire();
        }
        long t1 = System.nanoTime();
        t.join(5000);
        report("A Semaphore ping-pong ", t1 - t0, iters);
    }

    private static void parkRoundTrip(int iters) throws Exception {
        final Thread main = Thread.currentThread();
        final Thread[] holder = new Thread[1];
        final boolean[] ready = new boolean[1];
        Thread t = new Thread(() -> {
            for (int i = 0; i < iters; i++) {
                while (!ready[0]) {
                    LockSupport.park();
                }
                ready[0] = false;
                LockSupport.unpark(main);
            }
        });
        t.setDaemon(true);
        holder[0] = t;
        t.start();
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            ready[0] = true;
            LockSupport.unpark(holder[0]);
            LockSupport.park();
        }
        long t1 = System.nanoTime();
        t.join(5000);
        report("B park/unpark         ", t1 - t0, iters);
    }

    private static void monitorRoundTrip(int iters) throws Exception {
        final Object lock = new Object();
        final int[] turn = { 0 };
        Thread t = new Thread(() -> {
            synchronized (lock) {
                for (int i = 0; i < iters; i++) {
                    while (turn[0] != 1) {
                        try {
                            lock.wait();
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                    turn[0] = 0;
                    lock.notifyAll();
                }
            }
        });
        t.setDaemon(true);
        t.start();
        long t0 = System.nanoTime();
        synchronized (lock) {
            for (int i = 0; i < iters; i++) {
                turn[0] = 1;
                lock.notifyAll();
                while (turn[0] != 0) {
                    lock.wait();
                }
            }
        }
        long t1 = System.nanoTime();
        t.join(5000);
        report("C wait/notify         ", t1 - t0, iters);
    }

    static void report(String label, long nanos, int iters) {
        System.out.println(label + " total=" + (nanos / 1000000L) + "ms  round-trip="
                + (nanos / (double) iters / 1000.0) + "us");
    }
}
