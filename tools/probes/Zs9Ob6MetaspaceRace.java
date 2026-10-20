import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryUsage;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * zs s9/ob6 (rv6 X2): every {@code ZGC Major Cycles} notification carries a
 * measured {@code Metaspace} usage even while another thread keeps defining
 * classes (holding the class lock) around the collections. HotSpot never
 * reports an undefined (init = max = -1) usage. The listener is installed
 * before any memory-pool query, so the first collections take the
 * back-fill path (zs s6/ob5) that a busy class lock used to skip.
 *
 * <p>Stdout: shape facts only; counts and figures go to stderr.
 */
public class Zs9Ob6MetaspaceRace {
    static final int GCS = 20;

    /** Loaded once per throwaway loader. */
    public static class Payload {
        public int a(int x) { return x + 1; }
        public String b(String s) { return s + "b"; }
    }

    public static void main(String[] args) throws Exception {
        GarbageCollectorMXBean cycles = null;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (gc.getName().equals("ZGC Major Cycles")) {
                cycles = gc;
            }
        }
        List<String> shapes = new ArrayList<>();
        ((NotificationEmitter) cycles).addNotificationListener((n, h) -> {
            if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                return;
            }
            GcInfo gi = GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData()).getGcInfo();
            String s = shape(gi.getMemoryUsageBeforeGc().get("Metaspace"))
                    + "/" + shape(gi.getMemoryUsageAfterGc().get("Metaspace"));
            synchronized (shapes) {
                shapes.add(s);
            }
        }, null, null);
        URL here = Zs9Ob6MetaspaceRace.class.getProtectionDomain().getCodeSource().getLocation();
        AtomicBoolean stop = new AtomicBoolean();
        Thread definer = new Thread(() -> {
            int i = 0;
            while (!stop.get()) {
                try (URLClassLoader l = new URLClassLoader(new URL[] {here}, null)) {
                    l.loadClass("Zs9Ob6MetaspaceRace$Payload").getDeclaredMethods();
                    i++;
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
            }
            System.err.println("[probe] defined=" + i);
        }, "definer");
        definer.start();
        for (int g = 0; g < GCS; g++) {
            System.gc();
            Thread.sleep(5);
        }
        stop.set(true);
        definer.join();
        for (int i = 0; i < 500; i++) {
            synchronized (shapes) {
                if (shapes.size() >= GCS) {
                    break;
                }
            }
            Thread.sleep(10);
        }
        int undefined = 0, measured = 0;
        synchronized (shapes) {
            for (String s : shapes) {
                if (s.contains("undefined") || s.contains("absent")) {
                    undefined++;
                } else if (s.equals("measured/measured")) {
                    measured++;
                }
            }
            System.err.println("[probe] notifications=" + shapes.size() + " " + shapes);
            System.out.println("cycle_notifications_ge_" + GCS + "=" + (shapes.size() >= GCS));
            System.out.println("undefined_metaspace=" + undefined);
            System.out.println("all_measured=" + (measured == shapes.size()));
        }
        System.out.println("done");
    }

    static String shape(MemoryUsage u) {
        if (u == null) {
            return "absent";
        }
        if (u.getInit() == -1 && u.getMax() == -1 && u.getUsed() == 0) {
            return "undefined";
        }
        return u.getUsed() > 0 && u.getCommitted() >= u.getUsed() ? "measured" : "other";
    }
}
