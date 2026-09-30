// Which ROUTE to the direct buffer pool answers what.
//
// There are two, and they are not the same code in this VM:
//   A. ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)
//        -- intercepted by a CratonVM native (jmx.rs)
//   B. the platform MBean server, java.nio:type=BufferPool,name=direct
//        -- reached through DefaultPlatformMBeanProvider ->
//           ManagementFactoryHelper.getBufferPoolMXBeans(), the JDK own path
import java.lang.management.*;
import java.nio.ByteBuffer;
import java.util.*;
import javax.management.*;

public class PoolRoutes {
    static long a() {
        for (BufferPoolMXBean b : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class))
            if ("direct".equals(b.getName())) return b.getCount();
        return -1;
    }
    static long b() throws Exception {
        MBeanServer s = ManagementFactory.getPlatformMBeanServer();
        ObjectName n = new ObjectName("java.nio:type=BufferPool,name=direct");
        if (!s.isRegistered(n)) return -2;
        return ((Number) s.getAttribute(n, "Count")).longValue();
    }
    public static void main(String[] x) throws Exception {
        System.out.println("before  A(getPlatformMXBeans)=" + a() + "  B(MBeanServer)=" + b());
        ByteBuffer keep = ByteBuffer.allocateDirect(1 << 20);
        keep.put(0, (byte) 7);
        System.out.println("after   A(getPlatformMXBeans)=" + a() + "  B(MBeanServer)=" + b()
            + "  (kept " + keep.get(0) + ")");
        System.out.println("names in MBeanServer = "
            + new TreeSet<>(ManagementFactory.getPlatformMBeanServer()
                 .queryNames(new ObjectName("java.nio:type=BufferPool,*"), null).stream()
                 .map(ObjectName::toString).toList()));
        System.out.println("DONE");
    }
}
