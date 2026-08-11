import java.lang.management.ManagementFactory;

import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * Which JMX MXBean type mapping is in force?
 *
 * The real JDK machinery — the default since 2026-08-11 — makes
 * {@code MBeanServer.getAttribute("java.lang:type=Memory", "HeapMemoryUsage")}
 * return a {@code CompositeDataSupport}, as every other JVM does. The synthetic
 * overlay that {@code CRATONVM_SYNTHETIC_MXBEAN_MAPPING} restores types every
 * unrecognised Java type as {@code SimpleType.STRING} and converts nothing, so
 * the same call hands back a raw {@code java.lang.management.MemoryUsage}.
 *
 * That difference is visible from Java in one line, which makes it the cheap
 * check that the flag still reaches the code after being moved off a live
 * {@code std::env::var} and onto the latched {@code VmFlags} snapshot — a
 * declared flag read through {@code std::env} is unreachable from
 * {@code CRATONVM_REAL=-mxbean-mapping} and invisible to
 * {@code flags::with_thread_overrides}.
 *
 * Prints {@code PROBE mapping=real} or {@code PROBE mapping=synthetic}.
 */
public final class MxBeanMappingFlagProbe {

    public static void main(String[] args) throws Exception {
        MBeanServer server = ManagementFactory.getPlatformMBeanServer();
        Object usage = server.getAttribute(new ObjectName("java.lang:type=Memory"),
                "HeapMemoryUsage");
        boolean composite = usage instanceof javax.management.openmbean.CompositeData;
        System.out.println("PROBE mapping=" + (composite ? "real" : "synthetic")
                + " class=" + (usage == null ? "null" : usage.getClass().getName()));
    }
}
