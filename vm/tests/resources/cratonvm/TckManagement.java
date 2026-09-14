package cratonvm;

import java.lang.management.*;

/**
 * T4.6 conformance tests for java.management (java.lang.management).
 *
 * Exercises ManagementFactory accessors for platform MXBeans.
 * Every method returns 1 on pass, 0 on fail.
 */
public class TckManagement {

    public static int mbean_server() {
        try {
            return ManagementFactory.getPlatformMBeanServer() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int runtime_mxbean() {
        try {
            return ManagementFactory.getRuntimeMXBean() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int runtime_name() {
        try {
            return ManagementFactory.getRuntimeMXBean().getName() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int memory_mxbean() {
        try {
            return ManagementFactory.getMemoryMXBean() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int thread_mxbean() {
        try {
            return ManagementFactory.getThreadMXBean() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int thread_count() {
        try {
            return ManagementFactory.getThreadMXBean().getThreadCount() > 0 ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int classloading_mxbean() {
        try {
            return ManagementFactory.getClassLoadingMXBean() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int os_mxbean() {
        try {
            return ManagementFactory.getOperatingSystemMXBean() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int os_name() {
        try {
            return ManagementFactory.getOperatingSystemMXBean().getName() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
}
