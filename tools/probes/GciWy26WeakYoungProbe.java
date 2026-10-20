import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;

/**
 * gci i26/wy: variants of {@code WeakClearYoungProbe} that separate the
 * candidate causes of the Generational regression
 * ({@code docs/known-issues/gc/gci-i26wy-generational-young-pauses-keep-both-weak-referents-at-256m-20261015.md}).
 *
 * The two {@code WeakReference}s live in static fields (so no frame of the
 * spinning method holds them) and the spin loop is its own method. Variants
 * (argument 0):
 * <ul>
 * <li>{@code plain}: create both references, then spin.</li>
 * <li>{@code deep}: create both references 300 Java frames deep, return, then
 *     spin. Any native-stack word the interpreter left naming a referent lies
 *     far below the stack the spin loop and its collection helpers reuse.</li>
 * <li>{@code scrub}: as {@code plain}, then run a 400-frame recursion with
 *     long arguments before spinning, so the stack region the spin loop reuses
 *     is rewritten.</li>
 * </ul>
 * HotSpot 25 clears both referents at the first young collection in every
 * variant. stdout is deterministic (no allocation count); the allocation count
 * and the number of collections the loop saw go to stderr.
 */
public class GciWy26WeakYoungProbe {
    static volatile Object sink;
    static volatile long scrubSink;
    static WeakReference<Object> r0;
    static WeakReference<Object> r1;

    static WeakReference<Object> make() {
        return new WeakReference<>(new Object());
    }

    static void create() {
        r0 = new WeakReference<>(new Object());
        r1 = make();
    }

    static void createDeep(int depth) {
        if (depth > 0) {
            createDeep(depth - 1);
            return;
        }
        create();
    }

    static long scrub(int depth, long a, long b, long c) {
        if (depth == 0) {
            return a ^ b ^ c;
        }
        return scrub(depth - 1, a + 1, b + 2, c + 3) + 1;
    }

    static long spin() {
        long n = 0;
        while ((r0.get() != null || r1.get() != null) && n < 5_000_000L) {
            sink = new byte[256];
            n += 2;
        }
        return n;
    }

    static long gcCount() {
        long sum = 0;
        try {
            for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
                long c = b.getCollectionCount();
                if (c > 0) {
                    sum += c;
                }
            }
        } catch (Throwable t) {
            return -1;
        }
        return sum;
    }

    public static void main(String[] args) {
        String variant = args.length > 0 ? args[0] : "plain";
        long g0 = gcCount();
        if (variant.equals("deep")) {
            createDeep(300);
        } else {
            create();
        }
        if (variant.equals("scrub")) {
            scrubSink = scrub(400, 1L, 2L, 3L);
        }
        long n = spin();
        long g1 = gcCount();
        boolean c0 = r0.get() == null;
        boolean c1 = r1.get() == null;
        System.out.println("variant=" + variant
                + " inline=" + (c0 ? "cleared" : "LIVE")
                + " viaMethod=" + (c1 ? "cleared" : "LIVE")
                + ((c0 && c1) ? " PROBE-OK" : " PROBE-FAIL"));
        System.err.println("[wy26] variant=" + variant + " allocations=" + (n / 2)
                + " collections=" + ((g0 < 0 || g1 < 0) ? -1 : (g1 - g0)));
    }
}
