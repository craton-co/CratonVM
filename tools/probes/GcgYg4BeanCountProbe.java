/**
 * gcg g4/yg (2026-10-02): how the two Serial {@code GarbageCollectorMXBean}s
 * count a run whose young collections push old gen into majors without any
 * {@code System.gc()}.
 *
 * <p>Holds about 3/4 of {@code -Xmx} through a list of 64 KiB arrays (so old gen
 * fills and the collector runs majors), then churns 8 KiB garbage. stdout is
 * one deterministic line ({@code done beans}); the counts go to stderr
 * ({@code yg4 beans copy=<c> msc=<m>}), because how many collections each VM
 * runs differs. The row compares {@code copy} with the VM's own count of young
 * collections ({@code [GC] generational: minor=} under
 * {@code CRATONVM_DBG=gc-stats}): HotSpot books every young collection that ran
 * under {@code Copy}; a CratonVM young pause that also ran a major is booked
 * under {@code MarkSweepCompact} only
 * ({@code gcg-g4yg-young-sizing-and-xlog-review-findings-FIXED-20261015} item 6).
 * Run with {@code -Xmx64m}.
 */
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;

public class GcgYg4BeanCountProbe {
    static volatile Object sink;
    static List<byte[]> keep = new ArrayList<>();

    public static void main(String[] args) {
        long max = Runtime.getRuntime().maxMemory();
        long target = max / 4 * 3;
        long held = 0;
        while (held < target) {
            keep.add(new byte[64 * 1024]);
            held += 64 * 1024;
        }
        for (int i = 0; i < 40_000; i++) {
            sink = new byte[8192];
        }
        long copy = -1;
        long msc = -1;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (b.getName().equals("Copy")) {
                copy = b.getCollectionCount();
            } else if (b.getName().equals("MarkSweepCompact")) {
                msc = b.getCollectionCount();
            }
        }
        System.err.println("yg4 beans copy=" + copy + " msc=" + msc + " held=" + keep.size());
        System.out.println("done beans");
    }
}
