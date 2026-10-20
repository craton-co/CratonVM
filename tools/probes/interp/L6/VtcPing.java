// Virtual-thread continuations: the cost of one park/unpark switch between two
// virtual threads (`LockSupport.park` / `unpark` ping-pong), the row the
// runner hand-off spins of `vm/src/vm/jdk_continuation.rs` were tuned on.
// Prints wall milliseconds for `2 * n` switches, twice (the second warm).
//
//   javac -d /tmp/vtc tools/probes/interp/L6/VtcPing.java
//   java     -cp /tmp/vtc VtcPing 20000
//   cratonvm -cp /tmp/vtc VtcPing 20000
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;
public class VtcPing {
    public static void main(String[] a) throws Exception {
        final int n = Integer.parseInt(a[0]);
        for (int r = 0; r < 2; r++) {
            long t0 = System.nanoTime();
            AtomicInteger turn = new AtomicInteger();
            Thread[] p = new Thread[2];
            for (int k = 0; k < 2; k++) { final int me = k; p[k] = Thread.ofVirtual().unstarted(() -> {
                for (int i = 0; i < n; i++) { while (turn.get() % 2 != me) LockSupport.park(); turn.incrementAndGet(); LockSupport.unpark(p[1 - me]); } }); }
            p[0].start(); p[1].start(); p[0].join(); p[1].join();
            System.out.println("pingpong ms=" + (System.nanoTime() - t0) / 1_000_000);
        }
    }
}
