import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * zt t3/ob: the non-heap {@code init} figures, which are constants on
 * HotSpot 25 (x86_64, default segmented code cache): each
 * {@code CodeHeap '...'} pool 2555904 ({@code InitialCodeCacheSize}),
 * {@code Metaspace} and {@code Compressed Class Space} 0, and
 * {@code getNonHeapMemoryUsage().getInit()} their sum, 7667712. Stdout
 * prints the figures themselves (sorted by pool name).
 */
public class Zt3ObNonHeapInit {
    public static void main(String[] args) {
        List<String> lines = new ArrayList<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.NON_HEAP) {
                lines.add("pool " + p.getName() + " init=" + p.getUsage().getInit());
            }
        }
        Collections.sort(lines);
        for (String l : lines) {
            System.out.println(l);
        }
        System.out.println("nonheap_init=" + ManagementFactory.getMemoryMXBean().getNonHeapMemoryUsage().getInit());
        System.out.println("done");
    }
}
