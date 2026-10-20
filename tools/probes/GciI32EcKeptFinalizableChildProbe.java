import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;

/**
 * gci i32/ec (docs/known-issues/gc/gci-i32ec-g1-kept-finalizable-private-children-keep-their-weak-references-20261015.md):
 * a dead finalizable {@code Fin} allocated right beside a {@code Box} that a
 * compiled loop holds for its whole run (one TLAB, so one region, which a G1
 * young pause of this VM keeps in place while the compiled frame names the
 * box). {@code Fin} privately holds a {@code Child}; a {@code WeakReference}
 * to the child is the observable. HotSpot clears it at the pause that finds
 * {@code Fin} dead, before {@code finalize()} runs (finalizer-reachable is not
 * weakly reachable), whatever region either object is in.
 *
 * <p>Modes (second argument):
 * <ul>
 *   <li>{@code young}: the child is allocated beside the finalizable (the kept
 *       region too).</li>
 *   <li>{@code aged}: the child is allocated first and survives one young
 *       collection (a Survivor region), so the pinned pause COPIES it out of
 *       its collection set through the finalizable's slot.</li>
 *   <li>{@code agedhelper} (gci i38/ec): as {@code aged}, but the child is
 *       allocated and stored by {@code stashChild()}, so its address never
 *       passes through {@code main}'s (possibly compiled) frame. Same
 *       stdout as {@code aged} on HotSpot; on CratonVM it tells a stale
 *       compiled-frame word of {@code main} apart from the collector's
 *       answer.</li>
 *   <li>{@code reach}: the guard -- the finalizable is also held by a static
 *       array, so it is never finalized and the child's weak reference never
 *       cleared.</li>
 * </ul>
 *
 * <p>stdout (deterministic, HotSpot 25 with any collector): {@code young} /
 * {@code aged} / {@code agedhelper}: {@code fact child-cleared true}, {@code fact
 * cleared-before-finalize true}, {@code fact child-intact true}, {@code fact
 * warm-kept true}; {@code reach}: {@code fact reachable-kept true}, {@code fact
 * warm-kept true}. Diagnostics on stderr ({@code [ec32k] ...}).
 *
 * <p>Args: {@code [deadline seconds (default 10)] [young|aged|agedhelper|reach (default young)]}.
 */
public class GciI32EcKeptFinalizableChildProbe {
    static volatile boolean stop;
    static volatile long sinkLong;
    static volatile Object sinkObj;
    static volatile int finalized;
    static volatile boolean childIntact;
    static volatile boolean clearedBeforeFinalize;
    static volatile WeakReference<Child> childWeak;
    static final Object KEPT = new Object();
    static final Child WARM_CHILD = new Child();
    static Box pinStash;
    static Child childStash;
    /** reach mode: a static array holds the finalizable. */
    static final Object[] HOLD = new Object[1];

    static final class Box {
        int v = 7;
    }

    static final class Child {
        final int[] payload = new int[] {3, 5, 9};
    }

    static final class Fin {
        Child child;

        @Override
        @SuppressWarnings({"deprecation", "removal"})
        protected void finalize() {
            Child c = child;
            childIntact = c != null
                    && c.payload.length == 3
                    && c.payload[0] + c.payload[1] + c.payload[2] == 17;
            WeakReference<Child> w = childWeak;
            clearedBeforeFinalize = w != null && w.get() == null;
            finalized = finalized + 1;
        }
    }

    /** The referent never stays in the caller's frame: only the answer does. */
    static boolean gone(WeakReference<?> w) {
        return w.get() == null;
    }

    /**
     * The compiled loop: holds {@code pin} (and so its region) for its whole
     * run; returns 1 once the child's weak reference reads null, 2 if the
     * control reference to a static object does (never expected), 0 at the
     * deadline.
     */
    static int spin(Box pin, WeakReference<Child> wc, WeakReference<Object> k, long deadlineNanos) {
        long acc = 1;
        for (long i = 0; ; i++) {
            acc = acc * 6364136223846793005L + i;
            if ((i & 0xFFFFFL) == 0) {
                acc ^= pin.v;
                if (gone(k)) {
                    sinkLong = acc;
                    return 2;
                }
                if (gone(wc)) {
                    sinkLong = acc;
                    return 1;
                }
                if (System.nanoTime() > deadlineNanos) {
                    sinkLong = acc;
                    return 0;
                }
            }
        }
    }

    /** agedhelper: the child's address stays in this frame only. */
    static void stashChild() {
        childStash = new Child();
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = b.getCollectionCount();
            if (c > 0) {
                n += c;
            }
        }
        return n;
    }

    /** Allocate until at least one more collection has run (bounded). */
    static void oneCollection() {
        long before = gcCount();
        Object[] ring = new Object[64];
        int n = 0;
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (gcCount() == before && System.nanoTime() < deadline) {
            for (int i = 0; i < 1024; i++) {
                ring[n++ & 63] = new byte[2048];
            }
        }
        sinkObj = ring;
    }

    /**
     * The box, then at once the finalizable (one TLAB); in {@code young} mode
     * the child too. Only the box leaves this frame, through a static, and the
     * child's weak reference, through {@code childWeak}.
     */
    static void fresh(String mode) {
        Box pin = new Box();
        Fin f = new Fin();
        Child c = mode.startsWith("aged") ? childStash : new Child();
        childStash = null;
        f.child = c;
        childWeak = new WeakReference<>(c);
        if (mode.equals("reach")) {
            HOLD[0] = f;
        }
        pinStash = pin;
    }

    public static void main(String[] args) throws Exception {
        long seconds = args.length > 0 ? Long.parseLong(args[0]) : 10L;
        String mode = args.length > 1 ? args[1] : "young";
        boolean reach = mode.equals("reach");
        WeakReference<Object> k = new WeakReference<>(KEPT);
        WeakReference<Child> warmWeak = new WeakReference<>(WARM_CHILD);
        Box warmBox = new Box();
        for (int i = 0; i < 200; i++) {
            spin(warmBox, warmWeak, k, System.nanoTime() + 2_000_000L);
        }
        if (mode.equals("aged")) {
            childStash = new Child();
            oneCollection();
        } else if (mode.equals("agedhelper")) {
            stashChild();
            oneCollection();
        }
        Thread churn = new Thread(() -> {
            Object[] ring = new Object[64];
            int n = 0;
            while (!stop) {
                ring[n++ & 63] = new byte[2048];
            }
            sinkObj = ring;
        }, "churn");
        churn.setDaemon(true);
        fresh(mode);
        Box pin = pinStash;
        WeakReference<Child> wc = childWeak;
        pinStash = null;
        churn.start();
        long t0 = System.nanoTime();
        int result = spin(pin, wc, k, t0 + seconds * 1_000_000_000L);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        int waits = 0;
        int maxWaits = reach ? 6 : 200;
        while (finalized == 0 && waits < maxWaits) {
            System.runFinalization();
            Thread.sleep(50);
            waits++;
        }
        stop = true;
        churn.join();
        if (reach) {
            System.out.println("fact reachable-kept "
                    + (result == 0 && finalized == 0 && HOLD[0] != null && !gone(wc)));
        } else {
            System.out.println("fact child-cleared " + (result == 1));
            System.out.println("fact cleared-before-finalize " + (finalized == 1 && clearedBeforeFinalize));
            System.out.println("fact child-intact " + (finalized == 1 && childIntact));
        }
        System.out.println("fact warm-kept " + (result != 2 && !gone(k) && !gone(warmWeak)));
        System.err.println("[ec32k] mode=" + mode + " result=" + result + " elapsed_ms=" + ms
                + " finalize_waits=" + waits + " finalized=" + finalized
                + " cleared_before_finalize=" + clearedBeforeFinalize + " child_intact=" + childIntact
                + " pin=" + pin.v + " still_cleared=" + gone(wc) + " gcs=" + gcCount());
    }
}
