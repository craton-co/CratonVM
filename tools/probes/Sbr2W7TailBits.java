import java.lang.management.*;
import java.nio.*;
import java.util.*;

/** SBR2 W7 TAIL: direct-buffer pool counters (Bits.reserveMemory / Bits$1) as BufferPoolMXBean shows them. */
public class Sbr2W7TailBits {
    static BufferPoolMXBean pool() {
        for (var p : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) if (p.getName().equals("direct")) return p;
        return null;
    }
    static String snap() { var p = pool(); return "count=" + p.getCount() + " used=" + p.getMemoryUsed() + " cap=" + p.getTotalCapacity(); }
    public static void main(String[] a) throws Exception {
        System.out.println("start " + snap());
        var keep = new ArrayList<ByteBuffer>();
        for (int sz : new int[]{1, 64, 1024, 4096, 100000}) {
            var p0 = pool(); long c0 = p0.getCount(), u0 = p0.getMemoryUsed(), t0 = p0.getTotalCapacity();
            keep.add(ByteBuffer.allocateDirect(sz));
            var p1 = pool();
            System.out.println("alloc " + sz + " dcount=" + (p1.getCount() - c0) + " dused=" + (p1.getMemoryUsed() - u0) + " dcap=" + (p1.getTotalCapacity() - t0));
        }
        System.out.println("end " + snap().replaceAll("[0-9]+", "N"));
        System.out.println("pageAligned=" + Boolean.getBoolean("sun.nio.PageAlignDirectMemory"));
    }
}
