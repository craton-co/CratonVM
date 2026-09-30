import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;

/**
 * GC notification listeners never run on an application thread, as on HotSpot
 * (its `Notification Thread`). gc-common round 2026-09-24 (w6-d),
 * `docs/internal/gc-common-round-20260923/common-w3d-gc-notifications-run-on-the-allocating-mutator-FIXED-20260923.md`.
 *
 * <p>Registers a listener on every GC bean that records the name of the thread
 * it runs on, then allocates on a thread named `allocator` (holding a monitor
 * the listener also takes, the re-entrancy JLS 12.6 forbids for finalizers).
 * Prints `PROBE-OK` when at least one notification arrived and none ran on
 * `allocator` or `main`; `PROBE-FAIL` otherwise; `NO-NOTIFICATIONS` when the
 * backend sent none (only Generational describes GC beans today).
 */
public class GcNotificationThreadProbe {
    static final Object APP_LOCK = new Object();
    static final Set<String> THREADS = ConcurrentHashMap.newKeySet();
    static final AtomicInteger REENTERED = new AtomicInteger();
    static final AtomicInteger COUNT = new AtomicInteger();
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        NotificationListener listener = (n, h) -> {
            if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                return;
            }
            THREADS.add(Thread.currentThread().getName());
            if (Thread.holdsLock(APP_LOCK)) {
                REENTERED.incrementAndGet();
            }
            COUNT.incrementAndGet();
        };
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (gc instanceof NotificationEmitter) {
                ((NotificationEmitter) gc).addNotificationListener(listener, null, null);
            }
        }
        Thread allocator = new Thread(() -> {
            synchronized (APP_LOCK) {
                for (int i = 0; i < 64 * 1024; i++) {
                    sink = new byte[4 * 1024];
                }
            }
        }, "allocator");
        allocator.start();
        allocator.join();
        for (int i = 0; i < 50 && COUNT.get() == 0; i++) {
            Thread.sleep(20);
        }
        Thread.sleep(200);
        boolean onApp = THREADS.contains("allocator") || THREADS.contains("main");
        String verdict = COUNT.get() == 0 ? "NO-NOTIFICATIONS"
                : (!onApp && REENTERED.get() == 0 ? "PROBE-OK" : "PROBE-FAIL");
        // gce e1/o: stdout says only whether notifications arrived; the count
        // is the collector's collection count (HotSpot Serial 3, G1 2, ZGC 26;
        // Generational's young trigger fires at 50 % (moving) or 90 %
        // (non-moving) of a quarter-heap semi-space, where Serial's eden is
        // most of a third of the heap), which no two collectors share. The exact count
        // goes to stderr so the stdout line diffs clean against HotSpot.
        System.err.println("[probe] notifications=" + COUNT.get());
        System.out.println("notifications=" + (COUNT.get() > 0 ? "some" : "0")
                + " reentered=" + REENTERED.get()
                + " threads=" + new java.util.TreeSet<>(THREADS) + " " + verdict);
    }
}
