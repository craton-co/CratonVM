import java.lang.management.ManagementFactory;

/**
 * zt t3/ob: {@code com.sun.management.OperatingSystemMXBean}'s memory
 * figures are supported (not -1) and consistent, as on HotSpot 25 on
 * Linux and Windows. This VM answered -1 for every one of them on Windows
 * ({@code CRATONVM_WINDOWS_OS_MEMORY_METRICS}). Stdout carries relations
 * only; the figures go to stderr.
 */
public class Zt3ObOsMemoryMetrics {
    public static void main(String[] args) {
        com.sun.management.OperatingSystemMXBean os =
                (com.sun.management.OperatingSystemMXBean) ManagementFactory.getOperatingSystemMXBean();
        long total = os.getTotalMemorySize();
        long free = os.getFreeMemorySize();
        long swap = os.getTotalSwapSpaceSize();
        long freeSwap = os.getFreeSwapSpaceSize();
        long committed = os.getCommittedVirtualMemorySize();
        System.err.println("[probe] total=" + total + " free=" + free + " swap=" + swap
                + " free_swap=" + freeSwap + " committed_virtual=" + committed);
        System.out.println("total_memory_positive=" + (total > 0));
        System.out.println("free_memory_in_range=" + (free > 0 && free <= total));
        System.out.println("swap_defined=" + (swap >= 0 && freeSwap >= 0 && freeSwap <= swap));
        System.out.println("committed_virtual_positive=" + (committed > 0));
        System.out.println("legacy_names_agree=" + (os.getTotalPhysicalMemorySize() == total));
        System.out.println("done");
    }
}
