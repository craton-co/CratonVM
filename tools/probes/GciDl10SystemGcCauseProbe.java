import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gci i10/dl: the causes the GC notifications carry for ONE explicit
 * {@code System.gc()} on a quiet heap whose old generation holds regions that
 * mix live and dead objects (so G1's full collection has something to
 * compact). HotSpot (Serial and G1) reports the requester's cause only:
 * {@code [System.gc()]}. Before gci i10/dl CratonVM's G1 also reported
 * {@code Allocation Failure} for the compaction pause of its full collection
 * (gci-i9dc-g1-full-collection-compaction-pause-reports-allocation-failure-20261013).
 *
 * <p>stdout (deterministic):
 * <pre>
 * causes during explicit gc [System.gc()]
 * allocation failure during explicit gc false
 * kept 20000 first 0 last 19999
 * </pre>
 * stderr: {@code [gci-dl10] ...} with the collection counts and every cause
 * seen, in arrival order.
 */
public class GciDl10SystemGcCauseProbe {
    static final class Held10 {
        final long v;

        Held10(long v) {
            this.v = v;
        }
    }

    static final List<String> CAUSES = new ArrayList<>();
    static volatile boolean recording;

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = gc.getCollectionCount();
            if (c > 0) {
                n += c;
            }
        }
        return n;
    }

    static void listen() {
        NotificationListener l = (Notification n, Object handback) -> {
            if (!recording) {
                return;
            }
            if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                return;
            }
            GarbageCollectionNotificationInfo info =
                    GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
            synchronized (CAUSES) {
                CAUSES.add(info.getGcCause());
            }
        };
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (gc instanceof NotificationEmitter) {
                ((NotificationEmitter) gc).addNotificationListener(l, null, null);
            }
        }
    }

    static void pause(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    public static void main(String[] args) {
        gcCount(); // loads the management classes before the window
        listen();
        // Kept and dropped objects interleaved, so the old regions they are
        // promoted into hold both: after the drop each region keeps live
        // objects beside garbage (a compaction candidate, not a free region).
        int n = 20000;
        Held10[] kept = new Held10[n];
        Held10[] dropped = new Held10[n];
        for (int i = 0; i < n; i++) {
            kept[i] = new Held10(i);
            dropped[i] = new Held10(-i);
        }
        System.gc();
        System.gc();
        dropped = null;
        pause(300); // the set-up collections' notifications land before the window
        long c0 = gcCount();
        recording = true;
        System.gc();
        pause(500); // the explicit collection's notifications are delivered
        recording = false;
        long c1 = gcCount();
        List<String> seen;
        synchronized (CAUSES) {
            seen = new ArrayList<>(CAUSES);
        }
        TreeSet<String> causes = new TreeSet<>(seen);
        System.out.println("causes during explicit gc " + causes);
        System.out.println("allocation failure during explicit gc " + causes.contains("Allocation Failure"));
        System.out.println("kept " + kept.length + " first " + kept[0].v + " last " + kept[n - 1].v);
        System.err.println("[gci-dl10] collections_in_window=" + (c1 - c0) + " notifications=" + seen.size()
                + " seen=" + seen + " dropped_cleared=" + (dropped == null));
    }
}
