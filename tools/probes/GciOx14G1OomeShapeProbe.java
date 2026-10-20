import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

/**
 * gci i14/ox: the shape of a G1 heap OutOfMemoryError against HotSpot 25 G1
 * (page docs/internal/gc/gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018.md
 * items 3 and 7). HotSpot G1 answers an allocation that does not fit with one
 * young pause and then two {@code Pause Full (G1 Compaction Pause)} collections
 * before it throws: the {@code G1 Old Generation} bean's count rises by 2 per
 * error and two {@code end of major GC} notifications carry the cause
 * {@code G1 Compaction Pause}.
 *
 * <p>Modes (first argument; -Xmx64m assumed):
 * <ul>
 *   <li>{@code ladder}: two rounds; each holds 24 MiB of 64 KiB arrays and asks
 *       for one 48 MiB array, which cannot fit (one allocation-failure ladder
 *       per round). stderr per round: the bean count deltas over the failing
 *       request alone and the notifications it produced.</li>
 *   <li>{@code retry}: six rounds; each drops the previous round's 22 MiB live
 *       set (Old garbage), builds a new one (a concurrent cycle starts on the
 *       way: occupancy far above the IHOP) and then asks for an 8 MiB array,
 *       which fits once the dropped set is reclaimed. Every request is served
 *       on HotSpot. stderr: the bean count deltas over the whole mode.</li>
 * </ul>
 *
 * <p>stdout is deterministic and the same on HotSpot 25 G1:
 * <pre>
 * mode ladder
 * ladder round 1 oome Java heap space
 * ladder round 2 oome Java heap space
 * ladder after-drop ok
 * PROBE-OK
 * </pre>
 * ({@code retry}: {@code mode retry} / {@code retry served 6 of 6} /
 * {@code PROBE-OK}). Diagnostics on stderr, prefixed {@code [gci-ox14]}.
 */
public class GciOx14G1OomeShapeProbe {
    static final int MIB = 1 << 20;
    static final int CHUNK = 64 * 1024;
    static volatile Object sink;
    static List<byte[]> keep = new ArrayList<>();

    static final List<String> NOTES = new ArrayList<>();

    static TreeMap<String, Long> counts() {
        TreeMap<String, Long> m = new TreeMap<>();
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            m.put(b.getName(), b.getCollectionCount());
        }
        return m;
    }

    static long delta(TreeMap<String, Long> before, TreeMap<String, Long> after, String name) {
        Long a = after.get(name);
        Long b = before.get(name);
        return (a == null ? 0L : a) - (b == null ? 0L : b);
    }

    static String deltas(TreeMap<String, Long> before, TreeMap<String, Long> after) {
        StringBuilder sb = new StringBuilder();
        for (String name : after.keySet()) {
            sb.append(' ').append(name.replace(' ', '_')).append('=').append(delta(before, after, name));
        }
        return sb.toString();
    }

    static void listen() {
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            javax.management.NotificationListener l = (n, handback) -> {
                if (com.sun.management.GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION
                        .equals(n.getType())) {
                    com.sun.management.GarbageCollectionNotificationInfo info =
                            com.sun.management.GarbageCollectionNotificationInfo.from(
                                    (javax.management.openmbean.CompositeData) n.getUserData());
                    synchronized (NOTES) {
                        NOTES.add(info.getGcAction() + "/" + info.getGcCause());
                    }
                }
            };
            ((javax.management.NotificationEmitter) b).addNotificationListener(l, null, null);
        }
    }

    /** The notifications since the last call, as a sorted multiset; clears them. */
    static String takeNotes(int[] majorsOut) {
        TreeMap<String, Integer> m = new TreeMap<>();
        synchronized (NOTES) {
            for (String s : NOTES) {
                Integer c = m.get(s);
                m.put(s, c == null ? 1 : c + 1);
            }
            NOTES.clear();
        }
        StringBuilder sb = new StringBuilder();
        int majors = 0;
        for (Map.Entry<String, Integer> e : m.entrySet()) {
            if (sb.length() > 0) {
                sb.append(';');
            }
            sb.append(e.getKey().replace(' ', '_')).append('x').append(e.getValue());
            if (e.getKey().equals("end of major GC/G1 Compaction Pause")) {
                majors += e.getValue();
            }
        }
        majorsOut[0] = majors;
        return sb.length() == 0 ? "none" : sb.toString();
    }

    static void settle() {
        try {
            Thread.sleep(500); // notifications are delivered asynchronously
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    static void build(int mib) {
        keep = new ArrayList<>();
        int n = mib * (MIB / CHUNK);
        for (int i = 0; i < n; i++) {
            byte[] a = new byte[CHUNK];
            a[i % CHUNK] = 1;
            keep.add(a);
        }
    }

    static void ladder() {
        listen();
        for (int round = 1; round <= 2; round++) {
            build(24);
            settle();
            int[] majors = new int[1];
            takeNotes(majors);
            TreeMap<String, Long> before = counts();
            String msg;
            try {
                byte[] big = new byte[48 * MIB];
                big[big.length - 1] = 1;
                sink = big;
                msg = "served";
            } catch (OutOfMemoryError e) {
                msg = "oome " + e.getMessage();
            }
            sink = null;
            TreeMap<String, Long> after = counts();
            settle();
            String notes = takeNotes(majors);
            System.out.println("ladder round " + round + " " + msg);
            System.err.println("[gci-ox14] ladder round=" + round
                    + " young=" + delta(before, after, "G1 Young Generation")
                    + " old=" + delta(before, after, "G1 Old Generation")
                    + " conc=" + delta(before, after, "G1 Concurrent GC")
                    + " majors=" + majors[0]
                    + " notes=" + notes);
        }
        keep = new ArrayList<>();
        byte[] again = new byte[MIB];
        again[0] = 1;
        System.out.println("ladder after-drop " + (again[0] == 1 ? "ok" : "bad"));
    }

    static void retry() {
        TreeMap<String, Long> before = counts();
        int served = 0;
        int rounds = 6;
        for (int round = 1; round <= rounds; round++) {
            build(22);
            try {
                byte[] big = new byte[8 * MIB];
                big[big.length - 1] = 1;
                sink = big;
                served++;
            } catch (OutOfMemoryError e) {
                System.err.println("[gci-ox14] retry round=" + round + " oome " + e.getMessage());
            }
            sink = null;
        }
        keep = new ArrayList<>();
        TreeMap<String, Long> after = counts();
        System.out.println("retry served " + served + " of " + rounds);
        System.err.println("[gci-ox14] retry gc-deltas" + deltas(before, after));
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "ladder";
        System.out.println("mode " + mode);
        switch (mode) {
            case "ladder":
                ladder();
                break;
            case "retry":
                retry();
                break;
            default:
                System.out.println("unknown mode " + mode);
                return;
        }
        System.out.println("PROBE-OK");
    }
}
