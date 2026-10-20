// Probe (interpreter round i1 wave 11, lane L5): a registered native that
// answers a `synchronized` JDK method holds that method's monitor on EVERY
// call, including once the call site is inline-cached (interpreter) or
// site-cached (JIT). `Throwable.getCause()` is `public synchronized`; in
// `--compatible` CratonVM answers it with a registered native (a Bridge on the
// Throwable family), so the warmed call site below is served by a cached
// native entry. Until wave 11 such a site was never cached (every call took
// the slow path); a cache that dropped the monitor would print `false`.
//
// No setup needed. Run with and without --nojit.
//
// HotSpot 25 prints:
//   warm: cause=null
//   round 0: getCause waited for the holder: true
//   round 1: getCause waited for the holder: true
//   round 2: getCause waited for the holder: true
//   done
//
// stderr carries ns/call for a hot getCause() loop. To price the monitor,
// compare against CRATONVM_NATIVE_SYNC=0 on the same binary: the difference
// should be one uncontended lock/unlock, not a slow-path dispatch per call.
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;

public class SyncNativeCachedMonitor {
    // One call site for the warm-up, the rounds and the timing loop.
    static Throwable cause(Throwable t) {
        return t.getCause();
    }

    public static void main(String[] args) throws Exception {
        final Throwable t = new RuntimeException("probe");
        Throwable last = null;
        for (int i = 0; i < 50_000; i++) {
            last = cause(t);
        }
        System.out.println("warm: cause=" + last);

        for (int round = 0; round < 3; round++) {
            final AtomicBoolean released = new AtomicBoolean(false);
            final CountDownLatch held = new CountDownLatch(1);
            Thread holder = new Thread(() -> {
                synchronized (t) {
                    held.countDown();
                    try {
                        Thread.sleep(300);
                    } catch (InterruptedException e) {
                        // not expected
                    }
                    released.set(true);
                }
            });
            holder.start();
            held.await();
            // `getCause` must block until the holder leaves `synchronized (t)`.
            cause(t);
            System.out.println(
                    "round " + round + ": getCause waited for the holder: " + released.get());
            holder.join();
        }

        final int n = 2_000_000;
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            last = cause(t);
        }
        long t1 = System.nanoTime();
        System.err.println("getCause: " + ((t1 - t0) / (double) n) + " ns/call (last=" + last + ")");
        System.out.println("done");
    }
}
