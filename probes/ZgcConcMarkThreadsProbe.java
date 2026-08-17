import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/**
 * The multi-threaded half of {@code ZgcConcMarkProbe}: several mutator threads
 * allocating and storing while the concurrent marker traces, so the mark-start
 * safepoint has real peers to stop and the SATB barrier has real concurrency.
 *
 * <p>The single-threaded probe cannot exercise either. Its mark-start pause
 * takes an uncontended barrier with one registered thread, and every SATB
 * publication comes from the same thread that opened the cycle -- so a broken
 * takeover path, or an ingress that is not safe for concurrent publishers,
 * passes it.
 *
 * <p>The check at the end is a REACHABILITY walk, not a survivor count: every
 * slot each worker wrote must still point at an object whose fields read back
 * as what was written. A collector that freed a live object usually leaves the
 * reference intact and the memory zeroed, which a "did it survive" count reads
 * as a pass.
 *
 * <pre>
 *   java -XX:+UseZGC -Xmx1500m --verbose:gc ZgcConcMarkThreadsProbe 8 2000 200 25
 * </pre>
 *
 * args: threads, width, depth, rounds
 */
public class ZgcConcMarkThreadsProbe {
    /** Each element is a per-thread array of live chains. */
    static Object[][] retained;
    static final AtomicLong stores = new AtomicLong();
    static final AtomicLong badRefs = new AtomicLong();

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int width   = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        int depth   = args.length > 2 ? Integer.parseInt(args[2]) : 200;
        int rounds  = args.length > 3 ? Integer.parseInt(args[3]) : 25;

        retained = new Object[threads][];
        for (int t = 0; t < threads; t++) {
            retained[t] = new Object[width];
        }

        // Build the live set from the worker threads, so the graph is not all
        // allocated by one thread's TLAB chunks.
        CountDownLatch built = new CountDownLatch(threads);
        for (int t = 0; t < threads; t++) {
            final int me = t;
            new Thread(() -> {
                for (int i = 0; i < width; i++) {
                    Object[] head = new Object[4];
                    Object[] cur = head;
                    for (int d = 0; d < depth; d++) {
                        Object[] next = new Object[4];
                        next[0] = new byte[48];
                        cur[1] = next;
                        cur = next;
                    }
                    // Slot 3 is a self-check tag: it must still be `head` at
                    // the end, which a zeroed-out object would fail.
                    head[3] = head;
                    retained[me][i] = head;
                }
                built.countDown();
            }, "build-" + t).start();
        }
        built.await();
        System.out.println("live nodes ~" + ((long) threads * width * depth));

        long t0 = System.nanoTime();
        CountDownLatch done = new CountDownLatch(threads);
        for (int t = 0; t < threads; t++) {
            final int me = t;
            new Thread(() -> {
                for (int r = 0; r < rounds; r++) {
                    for (int k = 0; k < 20_000; k++) {
                        Object o = new byte[96];
                        if ((k & 7) == 0) {
                            Object[] head = (Object[]) retained[me][k % width];
                            head[2] = o;
                            stores.incrementAndGet();
                        }
                    }
                }
                done.countDown();
            }, "mutate-" + t).start();
        }
        done.await();
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        // Reachability walk. A freed-while-live object normally leaves the
        // reference in place and the memory zeroed, so the self-tag is what
        // actually catches it.
        long walked = 0;
        for (int t = 0; t < threads; t++) {
            for (int i = 0; i < width; i++) {
                Object[] head = (Object[]) retained[t][i];
                if (head == null || head[3] != head) { badRefs.incrementAndGet(); continue; }
                Object cur = head[1];
                int seen = 0;
                while (cur instanceof Object[] node) {
                    if (!(node[0] instanceof byte[] b) || b.length != 48) {
                        badRefs.incrementAndGet();
                        break;
                    }
                    seen++;
                    cur = node[1];
                }
                if (seen != depth) badRefs.incrementAndGet();
                walked += seen;
            }
        }
        System.out.println("threads=" + threads + " stores=" + stores.get()
                + " walked_nodes=" + walked + " BAD=" + badRefs.get()
                + " elapsed_ms=" + ms);
        if (badRefs.get() != 0) {
            System.out.println("FAIL: the collector damaged the live graph");
            System.exit(1);
        }
        System.out.println("OK");
    }
}
