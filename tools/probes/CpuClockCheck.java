import java.lang.management.ManagementFactory;

/** Does this VM support a CPU-time clock, and how coarse is it? */
public class CpuClockCheck {
    static long spin(long ms) {
        long t0 = System.nanoTime(), a = 0;
        while (System.nanoTime() - t0 < ms * 1_000_000L) a += (a * 31 + 7) ^ (a >>> 3);
        return a;
    }
    public static void main(String[] a) throws Exception {
        Object os = ManagementFactory.getOperatingSystemMXBean();
        System.out.println("CK osbean=" + os.getClass().getName());
        java.lang.reflect.Method m = null;
        try {
            m = os.getClass().getMethod("getProcessCpuTime");
            m.setAccessible(true);
        } catch (Throwable t) { System.out.println("CK getProcessCpuTime UNAVAILABLE: " + t); }
        var tb = ManagementFactory.getThreadMXBean();
        System.out.println("CK threadCpuTimeSupported=" + tb.isCurrentThreadCpuTimeSupported());
        for (int r = 0; r < 3; r++) {
            long p0 = m == null ? -1 : (Long) m.invoke(os);
            long c0 = tb.isCurrentThreadCpuTimeSupported() ? tb.getCurrentThreadCpuTime() : -1;
            long w0 = System.nanoTime();
            spin(500);
            long w1 = System.nanoTime();
            long c1 = tb.isCurrentThreadCpuTimeSupported() ? tb.getCurrentThreadCpuTime() : -1;
            long p1 = m == null ? -1 : (Long) m.invoke(os);
            System.out.printf("CK r%d wall=%.1fms threadCpu=%.1fms procCpu=%.1fms%n",
                r, (w1 - w0) / 1e6,
                c0 < 0 ? -1.0 : (c1 - c0) / 1e6,
                p0 < 0 ? -1.0 : (p1 - p0) / 1e6);
        }
    }
}
