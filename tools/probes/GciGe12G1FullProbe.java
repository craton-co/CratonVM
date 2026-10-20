import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i12/ge: how ONE {@code System.gc()} is reported, against HotSpot 25
 * (gci-i10dl-g1-full-collection-door-residuals-20261013 (b): HotSpot G1 reports
 * one {@code Pause Full (System.gc())}, one {@code G1 Old Generation} /
 * {@code end of major GC} notification, one JFR {@code G1Old} collection and
 * one {@code jdk.OldGarbageCollection}; gci-i11go-...-review-findings-20261013
 * item 3: the JFR young / old kind).
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code full}: an 8 MiB live set, then ONE {@code System.gc()}; prints
 *       each collector bean's count delta across the call (beans sorted by
 *       name) and the notifications whose cause is {@code System.gc()}.</li>
 *   <li>{@code fulljfr}: the same {@code System.gc()} inside a JFR recording
 *       of {@code jdk.GarbageCollection}, {@code jdk.OldGarbageCollection} and
 *       {@code jdk.YoungGarbageCollection}; prints how many collections carry
 *       the cause {@code System.gc()}, their names, and how many old / young
 *       events file under their ids.</li>
 * </ul>
 *
 * <p>stdout (deterministic; HotSpot 25 G1 at -Xmx256m):
 * <pre>
 * mode full
 * bean G1 Concurrent GC delta 0
 * bean G1 Old Generation delta 1
 * bean G1 Young Generation delta 0
 * system gc notifications 1
 * system gc notified by G1 Old Generation|end of major GC true
 * </pre>
 * ({@code fulljfr}: {@code fulljfr System.gc() collections 1},
 * {@code fulljfr System.gc() names [G1Old]}, {@code fulljfr System.gc() old
 * events 1}, {@code fulljfr System.gc() young events 0}.) stderr:
 * {@code [gci-ge12] ...} with every notification triple / JFR row seen.
 */
public class GciGe12G1FullProbe {
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

    static TreeMap<String, Long> counts() {
        TreeMap<String, Long> m = new TreeMap<>();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            m.put(gc.getName(), gc.getCollectionCount());
        }
        return m;
    }

    static void liveSet() {
        Object[] keep = new Object[2048];
        for (int i = 0; i < keep.length; i++) {
            keep[i] = new byte[4096];
        }
        sink = keep;
    }

    static void full() {
        liveSet();
        pause(500); // the live set's own collections (if any) are delivered first
        int already;
        synchronized (SEEN) {
            already = SEEN.size();
        }
        TreeMap<String, Long> before = counts();
        System.gc();
        pause(1000); // asynchronous delivery
        TreeMap<String, Long> after = counts();
        List<String> since;
        synchronized (SEEN) {
            since = new ArrayList<>(SEEN.subList(already, SEEN.size()));
        }
        for (Map.Entry<String, Long> e : after.entrySet()) {
            long was = before.containsKey(e.getKey()) ? before.get(e.getKey()) : 0L;
            System.out.println("bean " + e.getKey() + " delta " + (e.getValue() - was));
        }
        int sys = 0;
        TreeSet<String> sysBeans = new TreeSet<>();
        for (String t : since) {
            if (t.endsWith("|System.gc()")) {
                sys++;
                sysBeans.add(t.substring(0, t.length() - "|System.gc()".length()));
            }
        }
        System.out.println("system gc notifications " + sys);
        for (String b : sysBeans) {
            System.out.println("system gc notified by " + b + " true");
        }
        System.err.println("[gci-ge12] full triples=" + since + " before=" + before + " after=" + after);
    }

    static void fullJfr() throws Exception {
        liveSet();
        Path file = Files.createTempFile("gci-ge12-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.GarbageCollection");
            r.enable("jdk.OldGarbageCollection");
            r.enable("jdk.YoungGarbageCollection");
            r.start();
            System.gc();
            r.stop();
            r.dump(file);
        }
        Map<Integer, String> causeById = new HashMap<>();
        Map<Integer, String> nameById = new HashMap<>();
        List<Integer> oldIds = new ArrayList<>();
        List<Integer> youngIds = new ArrayList<>();
        TreeSet<String> rows = new TreeSet<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            String type = e.getEventType().getName();
            int id = e.getInt("gcId");
            if (type.equals("jdk.GarbageCollection")) {
                causeById.put(id, e.getString("cause"));
                nameById.put(id, e.getString("name"));
                rows.add("gc|" + id + "|" + e.getString("name") + "|" + e.getString("cause"));
            } else if (type.equals("jdk.OldGarbageCollection")) {
                oldIds.add(id);
                rows.add("old|" + id);
            } else if (type.equals("jdk.YoungGarbageCollection")) {
                youngIds.add(id);
                rows.add("young|" + id);
            }
        }
        Files.deleteIfExists(file);
        int collections = 0;
        TreeSet<String> names = new TreeSet<>();
        for (Map.Entry<Integer, String> e : causeById.entrySet()) {
            if ("System.gc()".equals(e.getValue())) {
                collections++;
                names.add(nameById.get(e.getKey()));
            }
        }
        int old = 0;
        for (int id : oldIds) {
            if ("System.gc()".equals(causeById.get(id))) {
                old++;
            }
        }
        int young = 0;
        for (int id : youngIds) {
            if ("System.gc()".equals(causeById.get(id))) {
                young++;
            }
        }
        System.out.println("fulljfr System.gc() collections " + collections);
        System.out.println("fulljfr System.gc() names " + names);
        System.out.println("fulljfr System.gc() old events " + old);
        System.out.println("fulljfr System.gc() young events " + young);
        System.err.println("[gci-ge12] fulljfr rows=" + rows);
    }

    /**
     * Three System.gc() inside a recording of jdk.SystemGC: how many events,
     * and the set of their invokedConcurrent values (gci i12/ge findings page
     * item 1: one event per request, the flag's value).
     */
    static void sysgcJfr() throws Exception {
        liveSet();
        Path file = Files.createTempFile("gci-ge12s-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.SystemGC");
            r.start();
            for (int i = 0; i < 3; i++) {
                System.gc();
            }
            r.stop();
            r.dump(file);
        }
        int events = 0;
        TreeSet<String> concurrent = new TreeSet<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            if (e.getEventType().getName().equals("jdk.SystemGC")) {
                events++;
                concurrent.add(String.valueOf(e.getBoolean("invokedConcurrent")));
            }
        }
        Files.deleteIfExists(file);
        System.out.println("sysgcjfr SystemGC events " + events);
        System.out.println("sysgcjfr invokedConcurrent " + concurrent);
    }

    /**
     * One System.gc() inside a recording of jdk.GarbageCollection: whether
     * every System.gc() collection's longestPause is at most its sumOfPauses
     * (stdout), and the two figures (stderr; findings page item 2).
     */
    static void fullJfr2() throws Exception {
        liveSet();
        Path file = Files.createTempFile("gci-ge12l-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.GarbageCollection");
            r.start();
            System.gc();
            r.stop();
            r.dump(file);
        }
        boolean within = true;
        List<String> figures = new ArrayList<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            if (e.getEventType().getName().equals("jdk.GarbageCollection")
                    && "System.gc()".equals(e.getString("cause"))) {
                long longest = e.getDuration("longestPause").toNanos();
                long sum = e.getDuration("sumOfPauses").toNanos();
                within &= longest <= sum;
                figures.add("longest=" + longest + " sum=" + sum);
            }
        }
        Files.deleteIfExists(file);
        System.out.println("fulljfr2 longestPause<=sumOfPauses " + within);
        System.err.println("[gci-ge12] fulljfr2 " + figures);
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "full";
        ManagementFactory.getGarbageCollectorMXBeans(); // loads the classes first
        listen();
        System.out.println("mode " + mode);
        if (mode.equals("fulljfr")) {
            fullJfr();
        } else if (mode.equals("sysgcjfr")) {
            sysgcJfr();
        } else if (mode.equals("fulljfr2")) {
            fullJfr2();
        } else {
            full();
        }
    }
}
