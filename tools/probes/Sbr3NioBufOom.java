import java.lang.management.*;
import java.nio.*;
import java.util.*;

/** SBR3 NIOBUF: the direct-memory cap (run with -XX:MaxDirectMemorySize=1m). Rows print "name |result|". */
public class Sbr3NioBufOom {
    static BufferPoolMXBean pool() {
        for (var p : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) if (p.getName().equals("direct")) return p;
        return null;
    }
    static void row(String n, Object v) { System.out.println(n + " |" + String.valueOf(v).replaceAll("[0-9]{3,}", "N") + "|"); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) { try { row(n, r.run()); } catch (Throwable e) { row(n, "THREW " + e.getClass().getName() + ": " + e.getMessage()); } }
    public static void main(String[] a) throws Exception {
        var keep = new ArrayList<ByteBuffer>();
        t("oom.big", () -> ByteBuffer.allocateDirect(2 * 1024 * 1024));
        t("oom.limit.exact", () -> keep.add(ByteBuffer.allocateDirect(1024 * 1024)));
        t("oom.after.full", () -> ByteBuffer.allocateDirect(1));
        t("oom.counters", () -> pool().getCount() + " " + (pool().getMemoryUsed() == 1024 * 1024) + " " + (pool().getTotalCapacity() == 1024 * 1024));
        keep.clear();
        t("oom.reclaim", () -> { for (int i = 0; i < 20; i++) ByteBuffer.allocateDirect(512 * 1024); return "ok"; });
        t("oom.counters2", () -> pool().getMemoryUsed() <= 1024 * 1024);
    }
}
