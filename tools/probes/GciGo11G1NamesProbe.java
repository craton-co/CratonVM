import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i11/go: the names and causes G1 reports for its collections, against
 * HotSpot 25 {@code -XX:+UseG1GC}
 * (gci-i10rh-g1-humongous-rset-and-mixed-choice-review-findings-20261013
 * findings 4-5, gci-i10dl-g1-full-collection-door-residuals-20261013 (a)).
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code young}: short-lived churn, young pauses only on HotSpot.</li>
 *   <li>{@code mixed}: an old generation that keeps turning over, so a
 *       concurrent cycle and mixed pauses follow (engagement is the -Xlog
 *       lines on stderr, judged by the list).</li>
 *   <li>{@code jfr}: the young churn inside a JFR recording of
 *       {@code jdk.GarbageCollection}.</li>
 *   <li>{@code sysgc}: ten {@code System.gc()} over a live set above the
 *       default IHOP (prints {@code system gc calls 10}; the list judges the
 *       {@code [GC] g1 mark-doors:} census on stderr).</li>
 *   <li>{@code jfr2}: the young churn inside a JFR recording of
 *       {@code jdk.G1GarbageCollection} and {@code jdk.ExecuteVMOperation}
 *       (the pause type and the VM operation name).</li>
 *   <li>{@code humongous}: humongous churn over a humongous live set above
 *       the default IHOP ({@code G1 Humongous Allocation}).</li>
 *   <li>{@code oome}: fill the heap with live arrays until
 *       {@code OutOfMemoryError}, then report whether a
 *       {@code G1 Compaction Pause} collection was notified.</li>
 * </ul>
 *
 * <p>stdout (deterministic; every line a boolean HotSpot G1 answers the same
 * way on every run):
 * <pre>
 * mode young
 * evacuation pause notified true
 * allocation failure notified false
 * </pre>
 * ({@code jfr}: {@code jfr G1New with G1 Evacuation Pause true},
 * {@code jfr allocation failure false}; {@code oome}: {@code oome thrown true},
 * {@code compaction pause cause notified true}, {@code allocation failure
 * notified false}.) stderr: {@code [gci-go11] ...} with every distinct
 * notification triple and JFR pair seen.
 */
public class GciGo11G1NamesProbe {
    static final List<String> SEEN = new ArrayList<>();
    static volatile Object sink;

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

    static TreeSet<String> seen() {
        synchronized (SEEN) {
            return new TreeSet<>(SEEN);
        }
    }

    static boolean anyCause(TreeSet<String> triples, String cause) {
        for (String t : triples) {
            if (t.endsWith("|" + cause)) {
                return true;
            }
        }
        return false;
    }

    /** About 512 MB of 1 KiB arrays with a 1000-array live window. */
    static void youngChurn() {
        Object[] window = new Object[1000];
        for (int i = 0; i < 512 * 1024; i++) {
            window[i % window.length] = new byte[1024];
        }
        sink = window;
    }

    /**
     * An old generation that turns over: 48 MB of 4 KiB arrays held in a
     * table, a quarter of it replaced per round, for 40 rounds, with
     * short-lived churn between rounds.
     */
    static void mixedChurn() {
        int slots = 12 * 1024;
        Object[] table = new Object[slots];
        for (int i = 0; i < slots; i++) {
            table[i] = new byte[4096];
        }
        long seed = 12345;
        for (int round = 0; round < 40; round++) {
            for (int k = 0; k < slots / 4; k++) {
                seed = seed * 6364136223846793005L + 1442695040888963407L;
                int at = (int) ((seed >>> 33) % slots);
                table[at] = new byte[4096];
            }
            Object[] window = new Object[256];
            for (int i = 0; i < 16 * 1024; i++) {
                window[i % window.length] = new byte[512];
            }
            sink = window;
        }
        sink = table;
    }

    /**
     * About 40 MB of 4 KiB arrays live (above G1's default 45 % IHOP at
     * -Xmx64m), a tenth of them replaced before each of ten System.gc().
     */
    static void sysgc() {
        int slots = 10 * 1024;
        Object[] table = new Object[slots];
        for (int i = 0; i < slots; i++) {
            table[i] = new byte[4096];
        }
        for (int round = 0; round < 10; round++) {
            for (int k = 0; k < slots / 10; k++) {
                table[round * (slots / 10) + k] = new byte[4096];
            }
            System.gc();
        }
        sink = table;
    }

    /**
     * 30 live 600 KiB arrays (humongous at 1 MiB regions: about 47 % of a
     * 64 MiB heap, over the default IHOP), then 2000 short-lived ones.
     */
    static void humongousChurn() {
        Object[] keep = new Object[30];
        for (int i = 0; i < keep.length; i++) {
            keep[i] = new byte[600 * 1024];
        }
        for (int i = 0; i < 2000; i++) {
            sink = new byte[600 * 1024];
        }
        sink = keep;
    }

    static boolean oome() {
        List<Object> live = new ArrayList<>();
        try {
            while (true) {
                live.add(new byte[64 * 1024]);
            }
        } catch (OutOfMemoryError e) {
            int n = live.size();
            live = null;
            sink = Integer.valueOf(n);
            return true;
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "young";
        ManagementFactory.getGarbageCollectorMXBeans(); // loads the classes first
        listen();
        System.out.println("mode " + mode);
        if (mode.equals("jfr")) {
            TreeSet<String> pairs = new TreeSet<>();
            Path file = Files.createTempFile("gci-go11-", ".jfr");
            try (Recording r = new Recording()) {
                r.enable("jdk.GarbageCollection");
                r.start();
                youngChurn();
                r.stop();
                r.dump(file);
            }
            for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
                pairs.add(e.getString("name") + "|" + e.getString("cause"));
            }
            Files.deleteIfExists(file);
            System.out.println("jfr G1New with G1 Evacuation Pause " + pairs.contains("G1New|G1 Evacuation Pause"));
            boolean af = false;
            for (String p : pairs) {
                af |= p.endsWith("|Allocation Failure");
            }
            System.out.println("jfr allocation failure " + af);
            System.err.println("[gci-go11] jfr pairs=" + pairs);
            return;
        }
        if (mode.equals("jfr2")) {
            TreeSet<String> rows = new TreeSet<>();
            Path file = Files.createTempFile("gci-go11b-", ".jfr");
            try (Recording r = new Recording()) {
                r.enable("jdk.G1GarbageCollection");
                r.enable("jdk.ExecuteVMOperation");
                r.start();
                youngChurn();
                r.stop();
                r.dump(file);
            }
            for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
                String type = e.getEventType().getName();
                if (type.equals("jdk.G1GarbageCollection")) {
                    rows.add("g1gc|" + e.getString("type"));
                } else if (type.equals("jdk.ExecuteVMOperation")) {
                    rows.add("op|" + e.getString("operation"));
                }
            }
            Files.deleteIfExists(file);
            System.out.println("jfr2 G1GarbageCollection Normal " + rows.contains("g1gc|Normal"));
            System.out.println("jfr2 operation G1CollectForAllocation " + rows.contains("op|G1CollectForAllocation"));
            System.err.println("[gci-go11] jfr2 rows=" + rows);
            return;
        }
        if (mode.equals("humongous")) {
            humongousChurn();
            pause(1000);
            TreeSet<String> t = seen();
            System.out.println("humongous allocation notified " + anyCause(t, "G1 Humongous Allocation"));
            System.out.println("allocation failure notified " + anyCause(t, "Allocation Failure"));
            System.err.println("[gci-go11] humongous triples=" + t);
            return;
        }
        if (mode.equals("oome")) {
            boolean thrown = oome();
            pause(1000); // the notifications of the last-ditch collections land
            TreeSet<String> t = seen();
            System.out.println("oome thrown " + thrown);
            System.out.println("compaction pause cause notified " + anyCause(t, "G1 Compaction Pause"));
            System.out.println("allocation failure notified " + anyCause(t, "Allocation Failure"));
            System.err.println("[gci-go11] oome triples=" + t);
            return;
        }
        if (mode.equals("sysgc")) {
            sysgc();
            pause(500);
            System.out.println("system gc calls 10");
            System.err.println("[gci-go11] sysgc triples=" + seen());
            return;
        }
        if (mode.equals("mixed")) {
            mixedChurn();
        } else {
            youngChurn();
        }
        pause(1000); // asynchronous delivery
        TreeSet<String> t = seen();
        System.out.println("evacuation pause notified "
                + t.contains("G1 Young Generation|end of minor GC|G1 Evacuation Pause"));
        System.out.println("allocation failure notified " + anyCause(t, "Allocation Failure"));
        System.err.println("[gci-go11] " + mode + " triples=" + t);
    }
}
