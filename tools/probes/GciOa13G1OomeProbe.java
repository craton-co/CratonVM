import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.SoftReference;
import java.util.List;
import java.util.TreeMap;

/**
 * gci i13/oa: the G1 allocation slow path and its OutOfMemoryError, against
 * HotSpot 25 G1 (page
 * docs/internal/gc/gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018.md).
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code headroom}: after {@code System.gc()}, a binary search (MiB
 *       granularity) for the largest {@code byte[]} that can be allocated
 *       (finding 1: the TLAB reserve refuses a humongous array HotSpot
 *       serves). stdout says only that the search ran and what the failing
 *       probes threw; the number is on stderr.</li>
 *   <li>{@code fill}: links 1 KiB nodes until {@code OutOfMemoryError}, catches
 *       it, drops the list and allocates again (the ordinary ladder).</li>
 *   <li>{@code humongous}: one array as large as the heap (refused), then an
 *       ordinary 1 MiB array (the humongous ladder; finding 5).</li>
 *   <li>{@code soft}: a SoftReference to a 4 MiB array, then a fill to
 *       {@code OutOfMemoryError}; the referent must be cleared before the
 *       error (java.lang.ref's guarantee; finding 2's soundness row).</li>
 *   <li>{@code vmlimit}: {@code new byte[Integer.MAX_VALUE]}.</li>
 *   <li>{@code nativehum}: 200 humongous {@code String[300000]} from the native
 *       {@code Array.newInstance}; prints whether any collection carried the
 *       cause {@code G1 Humongous Allocation} (item 4; HotSpot: true).</li>
 * </ul>
 *
 * <p>stdout is deterministic and the same on HotSpot 25 G1 (and Serial):
 * <pre>
 * mode headroom
 * headroom search done oome=Java heap space
 * </pre>
 * ({@code fill}: {@code fill oome Java heap space} / {@code fill after-drop ok};
 * {@code humongous}: {@code humongous oome Java heap space} /
 * {@code humongous after ok}; {@code soft}: {@code soft oome Java heap space} /
 * {@code soft cleared-before-oome true} / {@code soft after-drop ok};
 * {@code vmlimit}: {@code vmlimit oome Requested array size exceeds VM limit}.)
 * Then {@code PROBE-OK}. stderr: {@code [gci-oa13] ...} with the largest array,
 * the max heap and every collector bean's count delta over the mode.
 */
public class GciOa13G1OomeProbe {
    static final int MIB = 1 << 20;
    static volatile Object sink;

    static TreeMap<String, Long> counts() {
        TreeMap<String, Long> m = new TreeMap<>();
        List<GarbageCollectorMXBean> beans = ManagementFactory.getGarbageCollectorMXBeans();
        for (GarbageCollectorMXBean b : beans) {
            m.put(b.getName(), b.getCollectionCount());
        }
        return m;
    }

    static String deltas(TreeMap<String, Long> before, TreeMap<String, Long> after) {
        StringBuilder sb = new StringBuilder();
        for (String name : after.keySet()) {
            long b = before.containsKey(name) ? before.get(name) : 0L;
            sb.append(' ').append(name.replace(' ', '_')).append('=').append(after.get(name) - b);
        }
        return sb.toString();
    }

    /** One allocation of {@code mib} MiB (minus an array header); true = served. */
    static boolean tryMib(int mib, String[] lastOome) {
        try {
            byte[] a = new byte[mib * MIB - 16];
            a[a.length - 1] = 1;
            sink = a;
            sink = null;
            return true;
        } catch (OutOfMemoryError e) {
            lastOome[0] = e.getMessage();
            return false;
        }
    }

    static void headroom() {
        long max = Runtime.getRuntime().maxMemory();
        int lo = 0;
        int hi = (int) Math.min(2047L, max / MIB + 1);
        String[] lastOome = new String[] {"none"};
        System.gc();
        // Invariant: lo MiB was served (0 trivially), hi MiB was refused or is
        // above the heap.
        while (hi - lo > 1) {
            int mid = lo + (hi - lo) / 2;
            if (tryMib(mid, lastOome)) {
                lo = mid;
            } else {
                hi = mid;
            }
            System.gc();
        }
        // The heap-sized probe is always refused: one guaranteed message.
        if (tryMib((int) Math.min(2047L, max / MIB + 1), lastOome)) {
            lastOome[0] = "served-the-whole-heap";
        }
        System.out.println("headroom search done oome=" + lastOome[0]);
        System.err.println("[gci-oa13] headroom largestMiB=" + lo + " maxMiB=" + (max / MIB));
    }

    static final class Node {
        final long[] data = new long[126];
        Node next;
    }

    /**
     * Links 1 KiB nodes until the heap is full and returns the error's message
     * (an existing String: nothing is allocated between the error and the
     * return). The list dies with this frame, so the caller prints on a heap
     * with room.
     */
    static String fillUntilOome() {
        Node h = null;
        try {
            while (true) {
                Node n = new Node();
                n.next = h;
                h = n;
            }
        } catch (OutOfMemoryError e) {
            h = null;
            return e.getMessage();
        }
    }

    static void fill() {
        String msg = fillUntilOome();
        System.out.println("fill oome " + msg);
        byte[] again = new byte[MIB];
        again[0] = 1;
        System.out.println("fill after-drop " + (again.length == MIB ? "ok" : "bad"));
    }

    static void humongous() {
        long max = Runtime.getRuntime().maxMemory();
        int len = (int) Math.min(Integer.MAX_VALUE - 2L, max / 8 + 1024);
        try {
            long[] all = new long[len];
            sink = all;
            System.out.println("humongous served " + all.length);
        } catch (OutOfMemoryError e) {
            System.out.println("humongous oome " + e.getMessage());
        }
        sink = null;
        byte[] after = new byte[MIB];
        after[1] = 1;
        System.out.println("humongous after " + (after[1] == 1 ? "ok" : "bad"));
    }

    static void soft() {
        SoftReference<byte[]> ref = new SoftReference<>(new byte[4 * MIB]);
        String msg = fillUntilOome();
        // Read before any allocation: no collection can run in between.
        boolean cleared = ref.get() == null;
        System.out.println("soft oome " + msg);
        System.out.println("soft cleared-before-oome " + cleared);
        byte[] again = new byte[MIB];
        again[0] = 1;
        System.out.println("soft after-drop " + (again[0] == 1 ? "ok" : "bad"));
    }

    static final java.util.Set<String> CAUSES =
            java.util.Collections.synchronizedSet(new java.util.TreeSet<String>());

    static void listenCauses() {
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            javax.management.NotificationListener l = (n, handback) -> {
                if (com.sun.management.GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION
                        .equals(n.getType())) {
                    com.sun.management.GarbageCollectionNotificationInfo info =
                            com.sun.management.GarbageCollectionNotificationInfo.from(
                                    (javax.management.openmbean.CompositeData) n.getUserData());
                    CAUSES.add(info.getGcCause());
                }
            };
            ((javax.management.NotificationEmitter) b).addNotificationListener(l, null, null);
        }
    }

    /**
     * gci i13/oa item 4: humongous reference arrays from the NATIVE
     * {@code Array.newInstance} door (`Array.newArray`), churned until the
     * heap refuses one and the door collects: HotSpot names that collection
     * {@code G1 Humongous Allocation}.
     */
    static void nativeHumongous() {
        listenCauses();
        for (int i = 0; i < 200; i++) {
            Object a = java.lang.reflect.Array.newInstance(String.class, 300_000);
            sink = a;
        }
        sink = null;
        try {
            Thread.sleep(1000); // notifications are delivered asynchronously
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        boolean seen;
        String all;
        synchronized (CAUSES) {
            seen = CAUSES.contains("G1 Humongous Allocation");
            all = CAUSES.toString();
        }
        System.out.println("nativehum humongous-cause-seen " + seen);
        System.err.println("[gci-oa13] nativehum causes=" + all);
    }

    static void vmlimit() {
        try {
            byte[] a = new byte[Integer.MAX_VALUE];
            sink = a;
            System.out.println("vmlimit served");
        } catch (OutOfMemoryError e) {
            System.out.println("vmlimit oome " + e.getMessage());
        }
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "fill";
        System.out.println("mode " + mode);
        TreeMap<String, Long> before = counts();
        switch (mode) {
            case "headroom":
                headroom();
                break;
            case "fill":
                fill();
                break;
            case "humongous":
                humongous();
                break;
            case "soft":
                soft();
                break;
            case "vmlimit":
                vmlimit();
                break;
            case "nativehum":
                nativeHumongous();
                break;
            default:
                System.out.println("unknown mode " + mode);
                return;
        }
        TreeMap<String, Long> after = counts();
        System.err.println("[gci-oa13] " + mode + " gc-deltas" + deltas(before, after));
        System.out.println("PROBE-OK");
    }
}
