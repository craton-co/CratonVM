import java.util.concurrent.CountDownLatch;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

/**
 * Drives the `xt-helper-window-conservative-scan` moving-young fallback, which
 * the single-threaded {@code MovingYoungFallbackCallFormProbe} cannot produce
 * at all.
 *
 * That reason is marked when the helper-window pass finds a peer thread that is
 * BLOCKED while JIT return addresses are still on its native stack: such a
 * thread is excluded from the safepoint barrier, so its register file and raw
 * stack are scanned conservatively, and conservative roots are un-rewritable —
 * "this collection must not relocate" (`vm/src/jit/xt_root_scan.rs`).
 *
 * So the probe needs two things at once, and the shape matters more than the
 * workload:
 *
 *   1. peers parked UNDER compiled frames. Each worker warms {@link #hot} until
 *      it is compiled, then re-enters it with {@code block=true} so it parks at
 *      the bottom of a deep chain of those same compiled frames. Parking in an
 *      interpreted frame, or before the method is hot, produces nothing.
 *   2. a main thread allocating hard enough to force young collections while
 *      the peers sit there, so the collector actually meets that state.
 *
 * It prints the peer count and a per-run iteration total so a fallback rate can
 * be normalised per unit of work rather than compared as a raw count — the
 * comparison that matters when pricing a kill switch, since two arms rarely
 * complete the same amount of work in the same wall-clock.
 *
 * Usage: {@code MovingYoungFallbackPeerParkProbe [seconds] [peers] [depth]}
 */
public class MovingYoungFallbackPeerParkProbe {

    /** Warm-up passes per worker before it parks — enough to get {@link #hot} compiled. */
    private static final int WARMUP = 200_000;

    static final LinkedBlockingQueue<Object> RELEASE = new LinkedBlockingQueue<>();

    /**
     * Hot enough to be compiled, and deep enough that the park happens with a
     * long run of this method's own frames above it. The `block` flag is only
     * true on the final entry, so the warm-up passes stay allocation-light and
     * return promptly.
     */
    static long hot(int depth, boolean block) throws InterruptedException {
        if (depth <= 0) {
            if (block) {
                // Parks with WARMUP-compiled frames of `hot` on the native
                // stack — exactly the helper-window shape.
                RELEASE.take();
            }
            return 1;
        }
        return hot(depth - 1, block) + 1;
    }

    interface Node { long walk(int d); }

    static final class A implements Node {
        Node next;
        public long walk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 1; return j.length; }
            return next.walk(d - 1) + 1;
        }
    }
    static final class B implements Node {
        Node next;
        public long walk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 2; return j.length; }
            return next.walk(d - 1) + 2;
        }
    }
    static final class C implements Node {
        Node next;
        public long walk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 3; return j.length; }
            return next.walk(d - 1) + 3;
        }
    }
    static final class D implements Node {
        Node next;
        public long walk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 4; return j.length; }
            return next.walk(d - 1) + 4;
        }
    }

    /** Megamorphic chain, so the main thread also exercises the indirect-call reason. */
    static Node chain(int len) {
        Node[] all = new Node[len];
        for (int i = 0; i < len; i++) {
            switch (i % 4) {
                case 0:  all[i] = new A(); break;
                case 1:  all[i] = new B(); break;
                case 2:  all[i] = new C(); break;
                default: all[i] = new D(); break;
            }
        }
        for (int i = 0; i < len; i++) {
            Node n = all[(i + 1) % len], c = all[i];
            if (c instanceof A)      ((A) c).next = n;
            else if (c instanceof B) ((B) c).next = n;
            else if (c instanceof C) ((C) c).next = n;
            else                     ((D) c).next = n;
        }
        return all[0];
    }

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 30;
        int peers   = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        int depth   = args.length > 2 ? Integer.parseInt(args[2]) : 40;

        CountDownLatch parked = new CountDownLatch(peers);
        Thread[] workers = new Thread[peers];
        for (int i = 0; i < peers; i++) {
            workers[i] = new Thread(() -> {
                try {
                    long acc = 0;
                    for (int w = 0; w < WARMUP; w++) {
                        acc += hot(12, false);
                    }
                    if (acc == Long.MIN_VALUE) {
                        System.out.println("unreachable " + acc);
                    }
                    parked.countDown();
                    hot(12, true);          // park under compiled frames
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
            }, "peer-" + i);
            workers[i].setDaemon(true);
            workers[i].start();
        }

        // Only meaningful once the peers are actually parked; a run that starts
        // measuring while they are still warming measures the wrong state.
        if (!parked.await(120, TimeUnit.SECONDS)) {
            System.out.println("PROBE ERROR: peers did not park within 120s");
            return;
        }
        // The latch fires just BEFORE the blocking take(); give the parks a
        // moment to actually land on the queue.
        Thread.sleep(500);

        Node c = chain(depth + 2);
        long deadline = System.currentTimeMillis() + seconds * 1000L;
        long iters = 0, acc = 0;
        while (System.currentTimeMillis() < deadline) {
            for (int i = 0; i < 200; i++) {
                acc += c.walk(depth);
                iters++;
            }
        }

        for (int i = 0; i < peers; i++) {
            RELEASE.offer(new Object());
        }
        System.out.println("PROBE peers=" + peers + " depth=" + depth
                + " iters=" + iters + " acc=" + acc);
    }
}
