// Are the direct BufferPoolMXBean counters LIVE after the native was retired?
//
// DirectBufferCacheProbe asserts a DELTA, and a frozen counter passes it just as
// a working one does -- which is the exact failure mode `9d3f78943` was written
// against. So allocate direct buffers on purpose and see whether the numbers
// move. HotSpot is the oracle for the SHAPE (counts rise), not the exact value.
import java.lang.management.BufferPoolMXBean;
import java.lang.management.ManagementFactory;
import java.nio.ByteBuffer;
import java.util.List;

public class PoolLive {
    static BufferPoolMXBean direct() {
        for (BufferPoolMXBean b : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) {
            if ("direct".equals(b.getName())) return b;
        }
        return null;
    }

    public static void main(String[] args) {
        BufferPoolMXBean p = direct();
        if (p == null) {
            System.out.println("POOL direct=ABSENT");
            return;
        }
        long c0 = p.getCount(), u0 = p.getMemoryUsed();
        ByteBuffer[] keep = new ByteBuffer[8];
        for (int i = 0; i < keep.length; i++) keep[i] = ByteBuffer.allocateDirect(1 << 20);
        long c1 = p.getCount(), u1 = p.getMemoryUsed();
        System.out.println("POOL name=" + p.getName()
                + " count " + c0 + "->" + c1 + " (delta=" + (c1 - c0) + ")"
                + " used " + u0 + "->" + u1 + " (delta=" + (u1 - u0) + ")");
        System.out.println("POOL counters_live=" + ((c1 - c0) >= keep.length
                && (u1 - u0) >= (long) keep.length * (1 << 20)));
        System.out.println("POOL allbeans=" + ManagementFactory
                .getPlatformMXBeans(BufferPoolMXBean.class).size());
        List<BufferPoolMXBean> all = ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class);
        StringBuilder names = new StringBuilder();
        for (BufferPoolMXBean b : all) names.append(b.getName()).append(' ');
        System.out.println("POOL names=" + names.toString().trim());
    }
}
