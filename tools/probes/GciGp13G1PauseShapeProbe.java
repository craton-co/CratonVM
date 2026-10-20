import com.sun.management.GarbageCollectionNotificationInfo;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeMap;
import java.util.TreeSet;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gci i13/gp: two G1 pause shapes against HotSpot 25 {@code -XX:+UseG1GC}
 * (gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018 items 3 and 5).
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code abort}: a live old set of a 1.6M-node linked list (about half
 *       of {@code -Xmx128m}, above the default IHOP, and a chain the marker
 *       cannot split), then 40 rounds of about 8 MB of short-lived churn
 *       followed by ONE {@code System.gc()}. The first young pause of a round
 *       is a concurrent start, so a {@code System.gc()} often finds a cycle
 *       marking: HotSpot aborts it ({@code Concurrent Mark Abort}). The list
 *       judges the {@code -Xlog} lines on stderr.</li>
 *   <li>{@code meta}: 60000 defines of one ~4.5 KB class, each in a fresh
 *       loader dropped at once, under {@code -XX:MaxMetaspaceSize}: every
 *       define over the limit collects first ({@code Metadata GC
 *       Threshold}).</li>
 * </ul>
 *
 * <p>stdout (deterministic; HotSpot 25 G1):
 * <pre>
 * mode abort
 * abort system gc calls 40
 * abort list intact true
 * abort old collections per call true
 * </pre>
 * ({@code meta}: {@code meta completed true}, {@code meta metadata cause
 * notified true}, {@code meta list intact true}.) stderr: {@code [gci-gp13]
 * ...} with each collector bean's delta and every notification triple seen.
 */
public class GciGp13G1PauseShapeProbe {
    static final List<String> SEEN = new ArrayList<>();
    static volatile Object sink;

    static final int NODES = 1_600_000;
    static final int ROUNDS = 40;
    static final int DEFINES = 60_000;
    static final String NAME = "GciGp13G1PauseShapeProbe$Payload";

    static final String K =
        "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz!?";

    /** One ~4 KB constant (javac folds the concatenation into one Utf8 entry). */
    public static final class Payload {
        public static final String BLOB = K + K + K + K + K + K + K + K + K + K
            + K + K + K + K + K + K + K + K + K + K
            + K + K + K + K + K + K + K + K + K + K
            + K + K + K + K + K + K + K + K + K + K;
        public int touch() { return BLOB.length(); }
    }

    static final class OneShot extends ClassLoader {
        OneShot() { super(null); }
        Class<?> def(byte[] b) { return defineClass(NAME, b, 0, b.length); }
    }

    static final class Node {
        Node next;
        long a;
        long b;
    }

    static void listen() {
        NotificationListener l = (Notification n, Object handback) -> {
            if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                return;
            }
            GarbageCollectionNotificationInfo info =
                    GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
            synchronized (SEEN) {
                SEEN.add(info.getGcName() + "|" + info.getGcAction() + "|" + info.getGcCause());
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

    static TreeMap<String, Long> counts() {
        TreeMap<String, Long> m = new TreeMap<>();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            m.put(gc.getName(), gc.getCollectionCount());
        }
        return m;
    }

    static TreeMap<String, Long> deltas(TreeMap<String, Long> before, TreeMap<String, Long> after) {
        TreeMap<String, Long> d = new TreeMap<>();
        for (String name : after.keySet()) {
            Long b = before.get(name);
            d.put(name, after.get(name) - (b == null ? 0L : b.longValue()));
        }
        return d;
    }

    static TreeSet<String> seen() {
        synchronized (SEEN) {
            return new TreeSet<>(SEEN);
        }
    }

    static Node buildList() {
        Node head = null;
        for (int i = 0; i < NODES; i++) {
            Node n = new Node();
            n.next = head;
            n.a = i;
            head = n;
        }
        return head;
    }

    static boolean intact(Node head) {
        int n = 0;
        long expect = NODES - 1;
        for (Node at = head; at != null; at = at.next) {
            if (at.a != expect) {
                return false;
            }
            expect--;
            n++;
        }
        return n == NODES;
    }

    static void churn() {
        Object[] window = new Object[256];
        for (int i = 0; i < 32 * 1024; i++) {
            window[i % window.length] = new byte[256];
        }
        sink = window;
    }

    static byte[] payloadBytes() throws Exception {
        try (InputStream in = GciGp13G1PauseShapeProbe.class.getResourceAsStream(NAME + ".class")) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[8192];
            for (int n; (n = in.read(buf)) > 0; ) {
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        }
    }

    static void abort() {
        Node head = buildList();
        TreeMap<String, Long> before = counts();
        for (int round = 0; round < ROUNDS; round++) {
            churn();
            System.gc();
        }
        TreeMap<String, Long> d = deltas(before, counts());
        Long old = d.get("G1 Old Generation");
        System.out.println("abort system gc calls " + ROUNDS);
        System.out.println("abort list intact " + intact(head));
        System.out.println("abort old collections per call " + (old != null && old.longValue() == ROUNDS));
        pause(500);
        System.err.println("[gci-gp13] abort deltas=" + d);
        System.err.println("[gci-gp13] abort triples=" + seen());
        sink = head;
    }

    static void meta() throws Exception {
        Node head = buildSmallList();
        byte[] bytes = payloadBytes();
        TreeMap<String, Long> before = counts();
        int defined = 0;
        boolean completed;
        try {
            for (; defined < DEFINES; defined++) {
                OneShot loader = new OneShot();
                Class<?> c = loader.def(bytes);
                sink = c;
            }
            completed = true;
        } catch (OutOfMemoryError e) {
            completed = false;
            System.err.println("[gci-gp13] meta OutOfMemoryError: " + e.getMessage() + " after " + defined);
        }
        sink = null;
        pause(1000);
        TreeSet<String> t = seen();
        boolean metaCause = false;
        for (String s : t) {
            metaCause |= s.endsWith("|Metadata GC Threshold");
        }
        System.out.println("meta completed " + completed);
        System.out.println("meta metadata cause notified " + metaCause);
        System.out.println("meta list intact " + intactSmall(head));
        System.err.println("[gci-gp13] meta defines=" + defined + " deltas=" + deltas(before, counts()));
        System.err.println("[gci-gp13] meta triples=" + t);
    }

    static final int SMALL = 100_000;

    static Node buildSmallList() {
        Node head = null;
        for (int i = 0; i < SMALL; i++) {
            Node n = new Node();
            n.next = head;
            n.a = i;
            head = n;
        }
        return head;
    }

    static boolean intactSmall(Node head) {
        int n = 0;
        long expect = SMALL - 1;
        for (Node at = head; at != null; at = at.next) {
            if (at.a != expect) {
                return false;
            }
            expect--;
            n++;
        }
        return n == SMALL;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "abort";
        ManagementFactory.getGarbageCollectorMXBeans(); // loads the classes first
        listen();
        System.out.println("mode " + mode);
        if (mode.equals("meta")) {
            meta();
        } else {
            abort();
        }
    }
}
