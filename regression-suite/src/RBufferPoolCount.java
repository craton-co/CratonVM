// RBufferPoolCount — the direct buffer pool, through BOTH routes that reach it.
//
// There are two, and in this VM they were not the same code:
//
//   A. `ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)`
//   B. the platform MBean server, `java.nio:type=BufferPool,name=direct`,
//      which `DefaultPlatformMBeanProvider` reaches through
//      `ManagementFactoryHelper.getBufferPoolMXBeans()` — the JDK's own path,
//      and the one JConsole and every JMX exporter use.
//
// Route B read `java.nio.Bits`' three `AtomicLong`s, which `Bits.reserveMemory`
// maintains and which this VM never reaches, because `ByteBuffer
// .allocateDirect` is served by its own allocator. So B answered a constant
// zero in both modes while A moved — "a perfect cache no matter what the VM is
// doing", which is exactly what the record below argues a pool bean must never
// be. Route A, meanwhile, had no bean at all under `--jdk-only`.
//
// Assert the DELTA and the AGREEMENT, never an absolute: the byte counts differ
// legitimately (HotSpot's `getMemoryUsed` carries page-alignment slop), and the
// suite diffs this output against HotSpot's in the same environment.
//
//   docs/known-issues/jdk-only/
//     bug-the-bufferpool-refusal-takes-out-the-whole-platform-mbean-server-20260822.md
import java.lang.management.BufferPoolMXBean;
import java.lang.management.ManagementFactory;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;
import javax.management.MBeanServer;
import javax.management.ObjectName;

public class RBufferPoolCount {

    static int checks = 0;

    static void ck(String key, Object value) {
        checks++;
        System.out.println("CK RBufferPoolCount " + key + "=" + value);
    }

    static BufferPoolMXBean routeA(String name) {
        for (BufferPoolMXBean b : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) {
            if (name.equals(b.getName())) return b;
        }
        return null;
    }

    static long routeB(String name, String attr) throws Exception {
        MBeanServer s = ManagementFactory.getPlatformMBeanServer();
        ObjectName n = new ObjectName("java.nio:type=BufferPool,name=" + name);
        if (!s.isRegistered(n)) return -1;
        return ((Number) s.getAttribute(n, attr)).longValue();
    }

    public static void main(String[] args) throws Exception {
        // The list, and its ORDER — callers index it, so the order is part of
        // the answer.
        List<String> names = new ArrayList<>();
        for (BufferPoolMXBean b : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) {
            names.add(b.getName());
        }
        ck("routeA.pools", names);

        BufferPoolMXBean direct = routeA("direct");
        ck("routeA.directPresent", direct != null);
        ck("routeB.directRegistered", routeB("direct", "Count") >= 0);

        long a0 = direct == null ? -1 : direct.getCount();
        long b0 = routeB("direct", "Count");
        long au0 = direct == null ? -1 : direct.getMemoryUsed();
        long bu0 = routeB("direct", "MemoryUsed");

        ByteBuffer keep = ByteBuffer.allocateDirect(1 << 20);
        keep.put(0, (byte) 7);

        long a1 = direct == null ? -1 : direct.getCount();
        long b1 = routeB("direct", "Count");
        long au1 = direct == null ? -1 : direct.getMemoryUsed();
        long bu1 = routeB("direct", "MemoryUsed");

        ck("routeA.countMoved", a1 > a0);
        ck("routeB.countMoved", b1 > b0);
        ck("routeA.usedMoved", au1 > au0);
        ck("routeB.usedMoved", bu1 > bu0);
        // The two routes must not disagree about how many buffers are live.
        ck("routes.agreeBefore", a0 == b0);
        ck("routes.agreeAfter", a1 == b1);
        ck("kept", keep.get(0));

        // The other two pools are zero on a process that has mapped nothing —
        // on HotSpot too, so this is a diff and not an assertion about mmap.
        ck("pool.mapped.count", pool("mapped"));
        ck("pool.nonVolatile.count", pool("mapped - 'non-volatile memory'"));

        System.out.println("CK RBufferPoolCount checks=" + checks);
        System.out.println("PASS RBufferPoolCount (" + checks + " checks)");
    }

    static String pool(String name) {
        BufferPoolMXBean b = routeA(name);
        return b == null ? "absent" : String.valueOf(b.getCount());
    }
}
