import com.sun.management.ThreadMXBean;
import java.lang.management.ManagementFactory;

/**
 * The per-thread allocation counter is specified as monotonic for the life of
 * the thread. Allocate a KNOWN volume in a heap small enough to force many
 * collections and check the counter against arithmetic. A counter that loses
 * bytes across GC reports far less than it should, and reports differently at
 * different heap sizes for identical work.
 */
public class AllocCounterFidelity {
    static final ThreadMXBean TMX = (ThreadMXBean) ManagementFactory.getThreadMXBean();
    static Object sink;

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        int size  = a.length > 1 ? Integer.parseInt(a[1]) : 1024;   // bytes per array
        // warm
        for (int i = 0; i < 1000; i++) sink = new byte[size];
        long before = TMX.getCurrentThreadAllocatedBytes();
        for (int i = 0; i < iters; i++) sink = new byte[size];
        long after = TMX.getCurrentThreadAllocatedBytes();
        long reported = after - before;
        long expected = (long) iters * (size + 16);   // + array header
        System.out.println("iters=" + iters + " size=" + size);
        System.out.println("expected_MB=" + (expected / 1048576));
        System.out.println("reported_MB=" + (reported / 1048576));
        System.out.println("ratio_reported_over_expected=" + String.format("%.3f", reported / (double) expected));
        System.out.println("monotonic=" + (after >= before));
        System.out.println("FIDELITY_END");
    }
}
