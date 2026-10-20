import java.lang.management.*;
import java.util.*;

/** SBR2 W7 AWT: java.lang.management bean shapes, diffed against HotSpot. */
public class Sbr2W7AwtMgmt {
    static void p(String s) { System.out.println(s); }
    static void t(String n, java.util.concurrent.Callable<Object> c) {
        try { p(n + " = " + c.call()); } catch (Throwable e) { p(n + " THROWS " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    public static void main(String[] a) throws Exception {
        t("runtime", () -> ManagementFactory.getRuntimeMXBean().getClass().getName());
        t("spec", () -> ManagementFactory.getRuntimeMXBean().getSpecVersion());
        t("uptime>0", () -> ManagementFactory.getRuntimeMXBean().getUptime() > 0);
        t("pid>0", () -> ManagementFactory.getRuntimeMXBean().getPid() > 0);
        t("os", () -> ManagementFactory.getOperatingSystemMXBean().getClass().getName());
        t("os.name", () -> ManagementFactory.getOperatingSystemMXBean().getName().equals(System.getProperty("os.name")));
        t("os.procs", () -> ManagementFactory.getOperatingSystemMXBean().getAvailableProcessors() == Runtime.getRuntime().availableProcessors());
        t("cl", () -> ManagementFactory.getClassLoadingMXBean().getClass().getName());
        t("cl.loaded>0", () -> ManagementFactory.getClassLoadingMXBean().getLoadedClassCount() > 0);
        t("cl.total>=loaded", () -> ManagementFactory.getClassLoadingMXBean().getTotalLoadedClassCount() >= ManagementFactory.getClassLoadingMXBean().getLoadedClassCount());
        t("cl.verbose", () -> ManagementFactory.getClassLoadingMXBean().isVerbose());
        t("thread", () -> ManagementFactory.getThreadMXBean().getClass().getName());
        t("thread.count>0", () -> ManagementFactory.getThreadMXBean().getThreadCount() > 0);
        t("thread.contention-supported", () -> ManagementFactory.getThreadMXBean().isThreadContentionMonitoringSupported());
        t("thread.contention-enabled", () -> ManagementFactory.getThreadMXBean().isThreadContentionMonitoringEnabled());
        t("thread.contention-set", () -> { ManagementFactory.getThreadMXBean().setThreadContentionMonitoringEnabled(true); boolean r = ManagementFactory.getThreadMXBean().isThreadContentionMonitoringEnabled(); ManagementFactory.getThreadMXBean().setThreadContentionMonitoringEnabled(false); return r; });
        t("thread.cpu-supported", () -> ManagementFactory.getThreadMXBean().isThreadCpuTimeSupported() + " " + ManagementFactory.getThreadMXBean().isCurrentThreadCpuTimeSupported());
        t("thread.monitor-usage", () -> ManagementFactory.getThreadMXBean().isObjectMonitorUsageSupported() + " " + ManagementFactory.getThreadMXBean().isSynchronizerUsageSupported());
        t("thread.alloc", () -> ((com.sun.management.ThreadMXBean) ManagementFactory.getThreadMXBean()).isThreadAllocatedMemorySupported());
        t("runtime.bootcp", () -> ManagementFactory.getRuntimeMXBean().isBootClassPathSupported());
        t("comp.timemon", () -> { CompilationMXBean c = ManagementFactory.getCompilationMXBean(); return c.isCompilationTimeMonitoringSupported(); });
        t("mem.verbose", () -> { boolean v = ManagementFactory.getMemoryMXBean().isVerbose(); return v; });
        t("gc-names", () -> { StringBuilder sb = new StringBuilder(); for (GarbageCollectorMXBean g : ManagementFactory.getGarbageCollectorMXBeans()) sb.append(g.getName() != null).append(g.isValid()).append(' '); return sb; });
        t("comp", () -> { CompilationMXBean c = ManagementFactory.getCompilationMXBean(); return c == null ? "null" : c.getClass().getName(); });
        t("mem", () -> ManagementFactory.getMemoryMXBean().getClass().getName());
        t("mem.heap", () -> { MemoryUsage u = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage(); return (u.getUsed() > 0) + " " + (u.getCommitted() >= u.getUsed()) + " " + (u.getMax() == -1 || u.getMax() >= u.getCommitted()); });
        t("mem.nonheap", () -> ManagementFactory.getMemoryMXBean().getNonHeapMemoryUsage().getUsed() > 0);
        List<MemoryPoolMXBean> pools = ManagementFactory.getMemoryPoolMXBeans();
        p("pools>0=" + (pools.size() > 0));
        for (MemoryPoolMXBean m : pools) {
            p("pool type=" + m.getType() + " class=" + m.getClass().getName() + " nameNonEmpty=" + !m.getName().isEmpty()
              + " valid=" + m.isValid() + " mgrs>0=" + (m.getMemoryManagerNames().length > 0));
            try { MemoryUsage u = m.getUsage(); p("  usage-shape " + (u.getInit() >= -1) + " " + (u.getUsed() >= 0) + " " + (u.getCommitted() >= 0) + " " + (u.getMax() >= -1)); } catch (Throwable e) { p("  usage THROWS " + e); }
            try { m.getPeakUsage(); m.getCollectionUsage(); p("  peak/coll ok"); } catch (Throwable e) { p("  peak THROWS " + e); }
            p("  thresholdSupported=" + m.isUsageThresholdSupported() + " collThresholdSupported=" + m.isCollectionUsageThresholdSupported());
        }
        List<MemoryManagerMXBean> mgrs = ManagementFactory.getMemoryManagerMXBeans();
        p("mgrs>0=" + (mgrs.size() > 0));
        for (MemoryManagerMXBean m : mgrs) p("mgr class=" + m.getClass().getName() + " valid=" + m.isValid() + " pools>0=" + (m.getMemoryPoolNames().length > 0) + " nameNonEmpty=" + !m.getName().isEmpty());
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        p("gcs>0=" + (gcs.size() > 0));
        for (GarbageCollectorMXBean g : gcs) { System.gc(); p("gc class=" + g.getClass().getName() + " count>=0=" + (g.getCollectionCount() >= 0) + " time>=0=" + (g.getCollectionTime() >= 0) + " pools>0=" + (g.getMemoryPoolNames().length > 0)); }
        t("platform-server", () -> ManagementFactory.getPlatformMBeanServer().getMBeanCount() > 5);
        t("platform-ifaces>0", () -> ManagementFactory.getPlatformManagementInterfaces().size() > 0);
        t("runtime-oname", () -> ManagementFactory.getRuntimeMXBean().getObjectName());
        t("mem-oname", () -> ManagementFactory.getMemoryMXBean().getObjectName());
        t("gc-onames", () -> { for (GarbageCollectorMXBean g : gcs) g.getObjectName().getCanonicalName(); return "ok"; });
        t("server-attr", () -> ManagementFactory.getPlatformMBeanServer().getAttribute(new javax.management.ObjectName("java.lang:type=ClassLoading"), "LoadedClassCount") instanceof Integer);
        t("server-gcnames", () -> ManagementFactory.getPlatformMBeanServer().queryNames(new javax.management.ObjectName("java.lang:type=GarbageCollector,*"), null).size() == gcs.size());
        t("server-pools", () -> ManagementFactory.getPlatformMBeanServer().queryNames(new javax.management.ObjectName("java.lang:type=MemoryPool,*"), null).size() == pools.size());
    }
}
