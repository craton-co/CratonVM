import java.lang.management.*;
import java.util.*;
public class JmxProbe {
    public static void main(String[] a) {
        List<MemoryPoolMXBean> pools = ManagementFactory.getMemoryPoolMXBeans();
        List<MemoryManagerMXBean> mgrs = ManagementFactory.getMemoryManagerMXBeans();
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        System.out.println("pools=" + pools.size());
        for (MemoryPoolMXBean p : pools) System.out.println("  pool: " + p.getName() + " (" + p.getType() + ")");
        System.out.println("mgrs=" + mgrs.size());
        for (MemoryManagerMXBean m : mgrs) System.out.println("  mgr: " + m.getName());
        System.out.println("gcs=" + gcs.size());
        for (GarbageCollectorMXBean g : gcs) System.out.println("  gc: " + g.getName() + " count=" + g.getCollectionCount());
        System.out.println("OK");
    }
}
