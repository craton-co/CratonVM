import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

/**
 * gci i5/og (gcd-d4j half 2, the P-C major count on the thrash shape): the
 * phases of {@code GenR4W4HeapFullThrashProbe} (fill the heap with live data
 * until {@code OutOfMemoryError}, recover, four times), with the WORK of each
 * phase and the collections it took printed on stderr.
 *
 * vi2-og T measured more {@code MarkSweepCompact} collections with P-C than
 * without (86-143 vs 83-116 per row). If P-C frees floating garbage the legacy
 * seed kept, each phase allocates more before its OOME, and the extra majors
 * are work, not cost. This probe tells the two apart: a row's
 * {@code old_gcs / allocs} is the major count per unit of allocation.
 *
 * stdout is byte-identical to {@code GenR4W4HeapFullThrashProbe} (HotSpot diff);
 * every number is on stderr:
 *   {@code og5-work phase=<p> allocs=<n> old_gcs=<k> young_gcs=<j>}
 *   {@code og5-work total allocs=<n> old_gcs=<k> young_gcs=<j>}
 * {@code allocs} counts allocated objects (a phase's own plus its 200 000-node
 * recovery); {@code old_gcs} / {@code young_gcs} are the deltas of the
 * {@code MarkSweepCompact} / {@code Copy} beans over the phase.
 */
public final class GciOg5ThrashWorkProbe {
    static final class Node {
        final Node next;
        final long a, b, c, d;

        Node(Node next, long v) {
            this.next = next;
            this.a = v;
            this.b = v + 1;
            this.c = v + 2;
            this.d = v + 3;
        }
    }

    static final class Block {
        final Block next;
        final long[] data;

        Block(Block next) {
            this.next = next;
            this.data = new long[64];
        }
    }

    static Node head;
    static Block blocks;
    static volatile Node shared;
    static boolean ok = true;

    static final int RECOVER = 200_000;
    static long totalAllocs;
    static long[] lastGc = new long[] {0L, 0L};
    static final long[] FIRST_GC = new long[] {0L, 0L};

    static long[] gcCounts() {
        long old = 0;
        long young = 0;
        for (GarbageCollectorMXBean bean : ManagementFactory.getGarbageCollectorMXBeans()) {
            final long c = bean.getCollectionCount();
            if (c < 0) {
                continue;
            }
            if ("MarkSweepCompact".equals(bean.getName())) {
                old += c;
            } else if ("Copy".equals(bean.getName())) {
                young += c;
            }
        }
        return new long[] {old, young};
    }

    static void phaseEnd(String phase, long allocs) {
        final long[] now = gcCounts();
        totalAllocs += allocs;
        System.err.println("og5-work phase=" + phase + " allocs=" + allocs
                + " old_gcs=" + (now[0] - lastGc[0]) + " young_gcs=" + (now[1] - lastGc[1]));
        lastGc = now;
    }

    static void report(String what, OutOfMemoryError e) {
        final String msg = e.getMessage();
        System.out.println(what + ": OutOfMemoryError \"" + msg + "\"");
        ok &= "Java heap space".equals(msg);
    }

    /** The heap must be usable again: allocate and use ~10 MB of short-lived objects. */
    static void recovered(String what) {
        long sum = 0;
        for (int i = 0; i < RECOVER; i++) {
            final Node n = new Node(null, i);
            sum += n.d;
        }
        final boolean good = sum == 200_000L * 199_999L / 2 + 3L * 200_000L;
        System.out.println(what + "-recovered " + (good ? "ok" : "FAILED sum=" + sum));
        ok &= good;
    }

    /** One batch of the hot shape; small enough to be called many times. */
    static Node grow(Node from, long v, int n) {
        Node h = from;
        for (int i = 0; i < n; i++) {
            h = new Node(h, v + i);
        }
        return h;
    }

    public static void main(String[] args) throws Exception {
        final long[] start = gcCounts();
        FIRST_GC[0] = start[0];
        FIRST_GC[1] = start[1];
        lastGc = start;

        long chain = 0;
        try {
            while (true) {
                head = new Node(head, chain++);
            }
        } catch (OutOfMemoryError e) {
            head = null;
            report("chain", e);
        }
        recovered("chain");
        phaseEnd("chain", chain + RECOVER);

        long hot = 0;
        try {
            while (true) {
                head = grow(head, hot, 1024);
                hot += 1024;
            }
        } catch (OutOfMemoryError e) {
            head = null;
            report("chain-hot", e);
        }
        recovered("chain-hot");
        phaseEnd("chain-hot", hot + RECOVER);

        long arrays = 0;
        try {
            while (true) {
                blocks = new Block(blocks);
                arrays++;
            }
        } catch (OutOfMemoryError e) {
            blocks = null;
            report("arrays", e);
        }
        recovered("arrays");
        // A Block is two objects (the holder and its long[64]).
        phaseEnd("arrays", 2 * arrays + RECOVER);

        final AtomicInteger saw = new AtomicInteger();
        final AtomicInteger finished = new AtomicInteger();
        final AtomicLong threadAllocs = new AtomicLong();
        final Object lock = new Object();
        final Runnable filler = () -> {
            long v = 0;
            try {
                while (true) {
                    final Node n = new Node(null, v++);
                    synchronized (lock) {
                        shared = new Node(shared, n.a);
                    }
                }
            } catch (OutOfMemoryError e) {
                synchronized (lock) {
                    shared = null;
                }
                saw.incrementAndGet();
            } finally {
                // Two nodes per iteration (the scratch one and the shared one).
                threadAllocs.addAndGet(2 * v);
                finished.incrementAndGet();
            }
        };
        final Thread t1 = new Thread(filler, "filler-1");
        final Thread t2 = new Thread(filler, "filler-2");
        t1.start();
        t2.start();
        t1.join();
        t2.join();
        shared = null;
        final boolean threadsOk = saw.get() >= 1 && finished.get() == 2;
        System.out.println("threads: " + (saw.get() >= 1 ? "OutOfMemoryError seen" : "no OutOfMemoryError")
                + ", " + finished.get() + " of 2 finished");
        ok &= threadsOk;
        recovered("threads");
        phaseEnd("threads", threadAllocs.get() + RECOVER);

        final long[] end = gcCounts();
        System.err.println("og5-work total allocs=" + totalAllocs
                + " old_gcs=" + (end[0] - FIRST_GC[0]) + " young_gcs=" + (end[1] - FIRST_GC[1]));

        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
