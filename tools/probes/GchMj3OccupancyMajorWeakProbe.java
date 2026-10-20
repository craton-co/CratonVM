/**
 * gch h3/mj (2026-10-08): does the FIRST old collection that is not a
 * {@code System.gc()} clear a weak reference whose referent only a DEAD old
 * object reaches, through a YOUNG child?
 *
 * <p>HotSpot Serial's full GC marks from the true roots, so it does: at the end
 * of every {@code MarkSweepCompact} collection, a referent that was unreachable
 * when the collection began is cleared. A CratonVM Generational OCCUPANCY major
 * seeds its old mark from every young survivor (the legacy seed) unless
 * {@code CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS} (non-moving cycle) or
 * {@code CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS} (moving cycle) is on, and the young
 * phase keeps a young child a dead old holder's card names. So the target stays
 * set for one more major: the Java-visible half of
 * {@code gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism-20260928}
 * and the parity witness {@code gcd-d9a}'s STATUS asks for before either switch
 * flips. Requested majors (System.gc(), the allocation ladder) already take the
 * true roots, so the probe never calls System.gc() after its setup.
 *
 * <p>Shape, per round i: an old holder H_i (tenured during setup) gets a fresh
 * young child Y_i naming an old target T_i (tenured during setup, watched by a
 * WeakReference); then H_i and the strong T_i slot are dropped. Rounds run while
 * old gen fills through a retained list; a round is CHECKED when it was fully
 * armed before the first new MarkSweepCompact collection (the count read right
 * after the round still equals the baseline). When the count moves, every
 * checked round's weak reference must read null.
 *
 * <p>stdout: one line, {@code occupancy-major-weak: ok} (HotSpot, by
 * construction), {@code ... STALE} (a checked target survived the major) or
 * {@code ... void} (no major, or nothing checked). Counts go to stderr
 * ({@code mj3 omw checked= stale= msc_base= msc_end= rounds= held_kb=}).
 * Run with {@code -Xmx64m}; on CratonVM with
 * {@code CRATONVM_GC_NO_CONCURRENT_FIRST=1} so the old collection is the
 * Serial-shaped stop-the-world one.
 */
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

public class GchMj3OccupancyMajorWeakProbe {
    static final int PAIRS = 8192;

    static final class Holder {
        Object child;
    }

    static final class Child {
        final Object target;

        Child(Object target) {
            this.target = target;
        }
    }

    static final class Target {
        long a, b, c, d;
    }

    static Holder[] holders = new Holder[PAIRS];
    static Object[] targets = new Object[PAIRS];
    @SuppressWarnings("unchecked")
    static WeakReference<Object>[] weaks = new WeakReference[PAIRS];
    static boolean[] checked = new boolean[PAIRS];
    static List<byte[]> keep = new ArrayList<>();
    static volatile Object sink;
    static GarbageCollectorMXBean msc;

    static long mscCount() {
        return msc == null ? -1 : msc.getCollectionCount();
    }

    static void setUp() {
        for (int i = 0; i < PAIRS; i++) {
            holders[i] = new Holder();
            Target t = new Target();
            targets[i] = t;
            weaks[i] = new WeakReference<>(t);
        }
        // Tenure the pairs: one requested full collection, then enough young
        // churn to age anything a requested collection left young past every
        // tenuring threshold, then a second requested collection.
        System.gc();
        for (int i = 0; i < 800_000; i++) {
            sink = new byte[1024];
        }
        System.gc();
    }

    /** Round i: H_i -> fresh young Y_i -> T_i; then H_i and T_i's slot die. */
    static void arm(int i) {
        holders[i].child = new Child(targets[i]);
        targets[i] = null;
        holders[i] = null;
    }

    public static void main(String[] args) {
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (b.getName().equals("MarkSweepCompact")) {
                msc = b;
            }
        }
        setUp();
        long base = mscCount();
        long limit = Runtime.getRuntime().maxMemory() / 10 * 9;
        long held = 0;
        int rounds = 0;
        boolean major = false;
        while (held < limit) {
            if (rounds < PAIRS) {
                arm(rounds);
                if (mscCount() == base) {
                    checked[rounds] = true;
                }
                rounds++;
            }
            keep.add(new byte[16 * 1024]);
            held += 16 * 1024;
            for (int k = 0; k < 16; k++) {
                sink = new byte[2048];
            }
            if (mscCount() != base) {
                major = true;
                break;
            }
        }
        int nChecked = 0;
        int stale = 0;
        for (int i = 0; i < PAIRS; i++) {
            if (checked[i]) {
                nChecked++;
                if (weaks[i].get() != null) {
                    stale++;
                }
            }
        }
        long end = mscCount();
        keep.clear();
        System.err.println("mj3 omw checked=" + nChecked + " stale=" + stale + " msc_base=" + base
                + " msc_end=" + end + " rounds=" + rounds + " held_kb=" + (held / 1024)
                + " major=" + major);
        String verdict;
        if (!major || nChecked == 0) {
            verdict = "void";
        } else if (stale == 0) {
            verdict = "ok";
        } else {
            verdict = "STALE";
        }
        System.out.println("occupancy-major-weak: " + verdict);
    }
}
