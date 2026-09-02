// Companion to
// `bug-the-bufferpool-refusal-takes-out-the-whole-platform-mbean-server-20260822.md`.
//
// The record's point is that a refusal thrown from
// `VM$BufferPoolsHolder.<clinit>` is not scoped to buffer pools — it takes out
// `getPlatformMBeanServer()`, which every JMX consumer traverses. Keep the
// entry points separate so a regression names WHICH scope broke.
//
// The last two checks are the record's OTHER half: it argued that an empty
// list / constant-zero reading is the harm a loud refusal was chosen over.
// The fix that actually closed it (retiring `JavaNioAccess.getDirectBufferPool`
// so the JDK's own `Bits.BUFFER_POOL` bytecode runs) is supposed to give a
// direct query that MOVES. Allocating a direct buffer and reading the delta is
// the only check that can tell a working bean from a stub that answers 0.
//
//   javac -d probes/out probes/JmxBlast.java
//   java -cp probes/out JmxBlast
import java.lang.management.*;
import java.nio.ByteBuffer;
import java.util.*;
import javax.management.*;

public class JmxBlast {
    static int pass = 0, fail = 0;
    static int limit = 6;

    static void t(String label, Runnable r) {
        try { r.run(); System.out.println("PASS " + label); pass++; }
        catch (Throwable e) {
            System.out.println("FAIL " + label + " -> " + e);
            for (StackTraceElement s : e.getStackTrace()) {
                System.out.println("      at " + s);
                if (--limit <= 0) break;
            }
            limit = 6;
            fail++;
        }
    }

    static BufferPoolMXBean directPool() {
        for (BufferPoolMXBean b : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) {
            if ("direct".equals(b.getName())) return b;
        }
        return null;
    }

    public static void main(String[] a) throws Exception {
        t("getRuntimeMXBean().getName()",
            () -> Objects.requireNonNull(ManagementFactory.getRuntimeMXBean().getName()));
        t("getMemoryMXBean().getHeapMemoryUsage()",
            () -> Objects.requireNonNull(ManagementFactory.getMemoryMXBean().getHeapMemoryUsage()));
        t("getPlatformMXBeans(BufferPoolMXBean.class)", () -> {
            List<BufferPoolMXBean> l =
                ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class);
            System.out.println("      pools = " + l.size());
            for (BufferPoolMXBean b : l) {
                System.out.println("      pool " + b.getName()
                    + " count=" + b.getCount()
                    + " used=" + b.getMemoryUsed()
                    + " cap=" + b.getTotalCapacity());
            }
            if (l.isEmpty()) throw new IllegalStateException("no buffer pools at all");
        });
        // The blast radius: the standard entry point for ALL JMX registration.
        t("getPlatformMBeanServer()",
            () -> Objects.requireNonNull(ManagementFactory.getPlatformMBeanServer()));
        t("registerMBean + getAttribute", () -> {
            try {
                MBeanServer s = ManagementFactory.getPlatformMBeanServer();
                ObjectName n = new ObjectName("cratonvm.probe:type=Blast");
                if (!s.isRegistered(n)) s.registerMBean(new Blast(), n);
                Object v = s.getAttribute(n, "Answer");
                if (!Integer.valueOf(42).equals(v)) throw new IllegalStateException("got " + v);
            } catch (RuntimeException e) { throw e; }
              catch (Exception e) { throw new RuntimeException(e); }
        });
        // "A pool bean that always reports zero is worse than no pool bean —
        // it reports a perfect cache no matter what the VM is doing."
        t("direct pool bean exists", () -> Objects.requireNonNull(directPool(), "no 'direct' pool"));
        t("direct pool COUNTS a direct allocation", () -> {
            BufferPoolMXBean p = directPool();
            if (p == null) throw new IllegalStateException("no 'direct' pool");
            long c0 = p.getCount(), u0 = p.getMemoryUsed();
            ByteBuffer keep = ByteBuffer.allocateDirect(1 << 20);
            keep.put(0, (byte) 7);
            long c1 = p.getCount(), u1 = p.getMemoryUsed();
            System.out.println("      count " + c0 + " -> " + c1
                + ", used " + u0 + " -> " + u1 + " (kept " + keep.get(0) + ")");
            if (c1 <= c0) throw new IllegalStateException("count did not move: " + c0 + " -> " + c1);
            if (u1 <= u0) throw new IllegalStateException("used did not move: " + u0 + " -> " + u1);
        });
        System.out.println((fail == 0 ? "PASS" : "FAIL") + " JmxBlast "
            + pass + "/" + (pass + fail));
    }

    public interface BlastMBean { int getAnswer(); }
    public static class Blast implements BlastMBean {
        public int getAnswer() { return 42; }
    }
}
