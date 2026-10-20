/**
 * gci i4/yg: a compiled {@code byte[4096]} churn into a 256-slot ring (the shape of
 * {@code Gcd1JniRootsProbe}'s churn thread) beside a peer thread that stays in compiled code,
 * so the Generational young cycles run as non-moving sweeps ({@code cross-thread-jit-peer}).
 *
 * <p>i4/fj's O4: on that shape 83 % of the compiled TLAB refills found young unable to hold
 * the array and the array went straight to the old generation ({@code [GC] gen-alloc:}
 * {@code gen_old_allocs=} close to {@code tlab_refill_attempts - tlab_refill_ok}). HotSpot
 * Serial collects young on the failed eden allocation instead. This probe needs no JNI library,
 * so the deciding rows ({@code vi4-yg.list} block X) can compare the default with
 * {@code CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS=1} on any host.
 *
 * <p>Usage: {@code GciYg4ExhaustedRefillProbe [mib]} (default 4096 MiB churned). stdout is
 * deterministic (one line); the churn time goes to stderr as {@code [yg4] churn_ms=}.
 */
public class GciYg4ExhaustedRefillProbe {
    static final Object[] RING = new Object[256];
    static int ringAt;
    static volatile boolean stop;
    static volatile long peerSink;

    /** Short-lived 4 KiB arrays with a small live ring, as in Gcd1JniRootsProbe. */
    static void churn(int bytes) {
        for (int n = 0; n < bytes; n += 4096) {
            RING[ringAt++ & (RING.length - 1)] = new byte[4096];
        }
    }

    /** The peer's compiled loop: small arrays, so it refills too, and arithmetic. */
    static long peerWork(long seed) {
        long acc = seed;
        for (int i = 0; i < 10_000; i++) {
            int[] a = new int[4];
            a[i & 3] = i;
            acc = acc * 31 + a[i & 3];
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        int mib = args.length > 0 ? Integer.parseInt(args[0]) : 4096;
        Thread peer = new Thread(() -> {
            long s = 1;
            while (!stop) {
                s = peerWork(s);
            }
            peerSink = s;
        }, "yg4-peer");
        peer.setDaemon(true);
        peer.start();
        long t0 = System.nanoTime();
        for (int m = 0; m < mib; m++) {
            churn(1 << 20);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        stop = true;
        peer.join(30_000);
        int live = 0;
        for (Object o : RING) {
            if (o != null && ((byte[]) o).length == 4096) {
                live++;
            }
        }
        System.out.println("churned " + mib + " MiB, ring live=" + live + "/" + RING.length);
        System.err.println("[yg4] churn_ms=" + ms + " mib=" + mib);
    }
}
