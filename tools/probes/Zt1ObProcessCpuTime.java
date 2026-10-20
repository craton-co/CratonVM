import java.lang.management.ManagementFactory;

/**
 * zt t1/ob: {@code com.sun.management.OperatingSystemMXBean.getProcessCpuTime()}
 * is supported and advances with CPU work, as on HotSpot 25 (Linux and
 * Windows). This VM answered -1 on Windows ({@code CRATONVM_WINDOWS_PROCESS_CPU_TIME}).
 * Stdout carries only the two facts; the figures go to stderr.
 */
public class Zt1ObProcessCpuTime {
    static volatile long sink;

    public static void main(String[] args) {
        com.sun.management.OperatingSystemMXBean os =
                (com.sun.management.OperatingSystemMXBean) ManagementFactory.getOperatingSystemMXBean();
        long t0 = os.getProcessCpuTime();
        // About half a second of CPU on a slow box; the tick-granular Linux
        // figure (10 ms) moves well within it.
        long deadline = System.nanoTime() + 500_000_000L;
        long x = 1;
        while (System.nanoTime() < deadline) {
            for (int i = 0; i < 100_000; i++) {
                x = x * 6364136223846793005L + 1442695040888963407L;
            }
        }
        sink = x;
        long t1 = os.getProcessCpuTime();
        System.err.println("[probe] cpu_ns before=" + t0 + " after=" + t1);
        System.out.println("supported=" + (t0 >= 0 && t1 >= 0));
        System.out.println("advanced=" + (t1 > t0));
        System.out.println("done");
    }
}
