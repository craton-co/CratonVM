import java.lang.ref.Cleaner;
import java.lang.ref.WeakReference;
import java.util.WeakHashMap;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * zr z1/rf (2026-10-12): weak references, {@code WeakHashMap} keys and
 * {@code Cleaner} actions under collections the collector triggers itself (no
 * {@code System.gc()}), against HotSpot 25 {@code -XX:+UseZGC}.
 *
 * <p>Meant for CratonVM's concurrent ZGC mode ({@code CRATONVM_ZGC_CONC_START=auto}
 * or a percentage), where every collection can end in a concurrent mark: before
 * {@code CRATONVM_ZGC_CONC_REF_SKIP=1} the concurrent marker traced every referent
 * as a strong edge, so a cycle ending in a concurrent mark cleared none.
 *
 * <p>One mode. 1,000 weak references, 1,000 {@code WeakHashMap} keys and 1,000
 * {@code Cleaner} registrations to objects dropped at once; then about 2 GiB of
 * short-lived {@code byte[]} churn ({@code -Xmx64m}: dozens of cycles), checking
 * after every 64 MiB. HotSpot clears every one well before the end. Prints the
 * three verdicts; stderr carries the MiB of churn each took (diagnostic).
 */
public class Zr1RfConcWeakProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        final int n = 1000;
        @SuppressWarnings("unchecked")
        WeakReference<Object>[] refs = new WeakReference[n];
        WeakHashMap<Object, Integer> map = new WeakHashMap<>();
        Cleaner cleaner = Cleaner.create();
        AtomicInteger cleaned = new AtomicInteger();
        fill(refs, map, cleaner, cleaned);

        long weakAt = -1, mapAt = -1, cleanAt = -1;
        for (int mib = 0; mib < 2048; mib++) {
            sink = new byte[1 << 20];
            if ((mib & 63) == 63) {
                if (weakAt < 0 && allCleared(refs)) {
                    weakAt = mib;
                }
                if (mapAt < 0 && map.isEmpty()) {
                    mapAt = mib;
                }
                if (cleanAt < 0 && cleaned.get() == n) {
                    cleanAt = mib;
                }
                if (weakAt >= 0 && mapAt >= 0 && cleanAt >= 0) {
                    break;
                }
                Thread.sleep(1);
            }
        }
        sink = null;
        System.err.println("weak-at-mib=" + weakAt + " map-at-mib=" + mapAt + " clean-at-mib=" + cleanAt);
        System.out.println("weak cleared under churn: " + (weakAt >= 0));
        System.out.println("weakhashmap emptied under churn: " + (mapAt >= 0));
        System.out.println("cleaner ran under churn: " + (cleanAt >= 0));
        System.out.println("done");
    }

    static void fill(WeakReference<Object>[] refs, WeakHashMap<Object, Integer> map,
                     Cleaner cleaner, AtomicInteger cleaned) {
        for (int i = 0; i < refs.length; i++) {
            refs[i] = new WeakReference<>(new Object());
            map.put(new Object(), i);
            cleaner.register(new Object(), cleaned::incrementAndGet);
        }
    }

    static boolean allCleared(WeakReference<Object>[] refs) {
        for (WeakReference<Object> r : refs) {
            // `refersTo(null)`, not `get()`: a `get()` during a concurrent mark
            // is a keep-alive (HotSpot's and CratonVM's), so polling with it
            // would keep every referent alive for the cycle it lands in.
            if (!r.refersTo(null)) {
                return false;
            }
        }
        return true;
    }
}
