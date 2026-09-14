import com.sun.management.ThreadMXBean;
import java.lang.management.ManagementFactory;
import java.util.Locale;

/**
 * Does this VM's allocation accounting report a KNOWN volume correctly?
 *
 * BOTH counters are checked against the same ground truth, because they are
 * separate mechanisms and have failed separately:
 *
 *   getCurrentThreadAllocatedBytes()  per-thread, cumulative for the thread
 *   getTotalThreadAllocatedBytes()    process-wide, cumulative for the process
 *
 * The second is the one Hibernate's MemoryUsageUtil prefers, and therefore the
 * one HqlParserMemoryUsageTest's 256 MiB budget is asserted against. It read
 * 2.00x of retained heap on Generational and G1 and 3.00x on ZGC while the
 * per-thread counter beside it was exact -- two independent double-counts. An
 * earlier revision of this probe measured only the per-thread counter, which is
 * why a record built entirely on the process-wide one could be opened, argued
 * and half-closed without the instrument ever being suspected. Checking one
 * counter is not checking the accounting.
 *
 * Ground truth is RETAINED HEAP: allocate N objects of a known shape, hold
 * every one of them, and read the live set. A counter that reports more than
 * the bytes demonstrably still on the heap is over-reporting, whatever its
 * internal story is. The per-object figure is derived from the retained
 * measurement rather than assumed, so the probe carries no layout constant.
 *
 * Usage: AllocCounterFidelity [iters] [payload-longs]
 *
 * Exits 0 when both counters land within tolerance of ground truth and 1
 * otherwise, so this is runnable as a gate.
 */
public class AllocCounterFidelity {
    static final ThreadMXBean TMX = (ThreadMXBean) ManagementFactory.getThreadMXBean();

    /** How far from retained heap a counter may land and still be called exact. */
    static final double TOLERANCE = 0.08;

    static Object[] keep;

    public static void main(String[] a) throws Exception {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 1_000_000;
        int longs = a.length > 1 ? Integer.parseInt(a[1]) : 8;

        // Warm, unmeasured: class init, the counters' own first-call work and
        // the first TLAB refill all allocate.
        Object[] warm = new Object[1000];
        for (int i = 0; i < warm.length; i++) warm[i] = new long[longs];
        warm = null;

        Runtime rt = Runtime.getRuntime();
        // The retaining array is allocated BEFORE the window opens, so its own
        // bytes enter neither the counters nor the retained figure.
        Object[] hold = new Object[iters];
        keep = hold;

        settle(rt);
        long heapBefore = rt.totalMemory() - rt.freeMemory();
        long perThreadBefore = TMX.getCurrentThreadAllocatedBytes();
        long processBefore = TMX.getTotalThreadAllocatedBytes();

        for (int i = 0; i < iters; i++) hold[i] = new long[longs];

        long perThread = TMX.getCurrentThreadAllocatedBytes() - perThreadBefore;
        long process = TMX.getTotalThreadAllocatedBytes() - processBefore;
        settle(rt);
        long retained = (rt.totalMemory() - rt.freeMemory()) - heapBefore;

        // Reachable until here, so nothing measured above can have been collected.
        if (keep.length != iters) throw new IllegalStateException("unreachable");

        double truth = retained / (double) iters;
        double pt = perThread / (double) iters;
        double pr = process / (double) iters;

        System.out.println("iters=" + iters + " shape=long[" + longs + "]");
        System.out.println(String.format(Locale.ROOT, "%-46s %8.1f",
                "retained heap per object (ground truth)", truth));
        System.out.println(String.format(Locale.ROOT, "%-46s %8.1f  ratio %.2f",
                "getCurrentThreadAllocatedBytes (per-thread)", pt, pt / truth));
        System.out.println(String.format(Locale.ROOT, "%-46s %8.1f  ratio %.2f",
                "getTotalThreadAllocatedBytes (process-wide)", pr, pr / truth));

        boolean ptOk = within(pt, truth);
        boolean prOk = within(pr, truth);
        System.out.println("per_thread=" + (ptOk ? "OK" : "OFF"));
        System.out.println("process_wide=" + (prOk ? "OK" : "OFF"));
        System.out.println("monotonic=" + (perThread >= 0 && process >= 0));

        keep = null;
        hold = null;
        boolean churnOk = churnPhase(iters, longs, truth);

        System.out.println("FIDELITY_END");
        if (!ptOk || !prOk || !churnOk) System.exit(1);
    }

    /**
     * PHASE 2 -- the same volume, none of it retained.
     *
     * Phase 1 cannot fail the way this record's counters actually failed. It
     * holds every object, so the measured window contains little or no
     * collection, and the two defects this probe exists to catch (a counter
     * that moves at a GC; a span counted once by the thread and again by a
     * heap-internal layer that only retires under collection pressure) are
     * both quietest exactly there. A counter can be perfect on phase 1 and
     * still report 2.6x on a real parse -- which is what the ORM suite's G1
     * arm did.
     *
     * Ground truth here is ARITHMETIC, not retention: `perObject` was
     * established by phase 1's retained measurement, so `iters * perObject` is
     * the volume this loop allocates whether or not the collector reclaims it
     * underneath. That is not circular -- the size comes from the heap, only
     * the multiplication comes from the loop.
     */
    static Object churnSink;

    static boolean churnPhase(int iters, int longs, double perObject) {
        for (int i = 0; i < 1000; i++) churnSink = new long[longs];
        long ptBefore = TMX.getCurrentThreadAllocatedBytes();
        long prBefore = TMX.getTotalThreadAllocatedBytes();
        for (int i = 0; i < iters; i++) churnSink = new long[longs];
        long pt = TMX.getCurrentThreadAllocatedBytes() - ptBefore;
        long pr = TMX.getTotalThreadAllocatedBytes() - prBefore;
        double ptPer = pt / (double) iters;
        double prPer = pr / (double) iters;
        System.out.println("-- churn phase (nothing retained, collector runs inside the window) --");
        System.out.println(String.format(Locale.ROOT, "%-46s %8.1f",
                "expected per object (from phase 1)", perObject));
        System.out.println(String.format(Locale.ROOT, "%-46s %8.1f  ratio %.2f",
                "getCurrentThreadAllocatedBytes (per-thread)", ptPer, ptPer / perObject));
        System.out.println(String.format(Locale.ROOT, "%-46s %8.1f  ratio %.2f",
                "getTotalThreadAllocatedBytes (process-wide)", prPer, prPer / perObject));
        boolean a = within(ptPer, perObject);
        boolean b = within(prPer, perObject);
        System.out.println("churn_per_thread=" + (a ? "OK" : "OFF"));
        System.out.println("churn_process_wide=" + (b ? "OK" : "OFF"));
        return a && b;
    }

    static boolean within(double got, double truth) {
        return truth > 0 && Math.abs(got - truth) / truth <= TOLERANCE;
    }

    /**
     * Bring the live-set reading to rest. Two collections, because the first
     * can leave a just-promoted set uncounted on a generational collector.
     */
    static void settle(Runtime rt) throws Exception {
        for (int i = 0; i < 2; i++) {
            rt.gc();
            Thread.sleep(50);
        }
    }
}
