import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.List;

/**
 * gci i47w/sg47: after ONE {@code System.gc()} is live young data old-generation data? HotSpot 25's explicit
 * collection is a full one (Serial: mark-compact of the whole heap, young slid into Tenured; G1: Full GC compacting
 * into old regions), so the 4 MiB of {@code byte[]} kept below reads in the old pool, the survivor pool is empty and
 * eden's collection usage is 0. CratonVM: under {@code CRATONVM_GC_SYSTEM_GC_TENURES_ALL}
 * (page {@code gci-i47w-j46-system-gc-leaves-live-young-data-out-of-the-old-pool-20261016}).
 *
 * <p>stdout = predicates only ({@code sg47 ...}, deterministic); figures on stderr ({@code [sg47] ...}).
 */
public class GciSg47SystemGcTenuresProbe {
    static final List<Object> KEEP = new ArrayList<>();
    static final int ARRAYS = 64;
    static final int ARRAY_BYTES = 64 * 1024;

    public static void main(String[] args) {
        MemoryPoolMXBean old = null;
        MemoryPoolMXBean survivor = null;
        MemoryPoolMXBean eden = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.HEAP) {
                continue;
            }
            String name = p.getName();
            if (name.contains("Old") || name.contains("Tenured")) {
                old = p;
            } else if (name.contains("Survivor")) {
                survivor = p;
            } else if (name.contains("Eden")) {
                eden = p;
            }
        }
        boolean found = old != null && survivor != null && eden != null;
        System.out.println("sg47 pools-found=" + found);
        if (!found) {
            System.out.println("sg47 done");
            return;
        }
        for (int i = 0; i < ARRAYS; i++) {
            KEEP.add(new byte[ARRAY_BYTES]);
        }
        System.gc();
        long oldUsed = old.getUsage().getUsed();
        long survivorUsed = survivor.getUsage().getUsed();
        MemoryUsage edenAfter = eden.getCollectionUsage();
        long edenCollectionUsed = edenAfter == null ? -1 : edenAfter.getUsed();
        MemoryUsage survivorAfter = survivor.getCollectionUsage();
        long survivorCollectionUsed = survivorAfter == null ? -1 : survivorAfter.getUsed();
        System.err.println("[sg47] " + old.getName() + " used=" + oldUsed + " " + survivor.getName() + " used="
                + survivorUsed + " collection-used=" + survivorCollectionUsed + " " + eden.getName()
                + " collection-used=" + edenCollectionUsed + " keep=" + KEEP.size());

        System.out.println("sg47 old-used-positive=" + (oldUsed > 0));
        System.out.println("sg47 old-holds-keep=" + (oldUsed >= (long) ARRAYS * ARRAY_BYTES));
        System.out.println("sg47 survivor-used-zero=" + (survivorUsed == 0));
        System.out.println("sg47 survivor-collection-used-zero=" + (survivorCollectionUsed == 0));
        System.out.println("sg47 eden-collection-used-zero=" + (edenCollectionUsed == 0));

        // The usage threshold the old pool can now trip (HotSpot: the set itself detects the crossing).
        old.setUsageThreshold(1);
        System.out.println("sg47 old-threshold-exceeded=" + old.isUsageThresholdExceeded());
        System.out.println("sg47 keep=" + KEEP.size());
        System.out.println("sg47 done");
    }
}
