import java.lang.ref.WeakReference;

/**
 * Companion to {@code WeakClearYoungProbe}: the same allocation-driven weak
 * clear, with the one difference that the loop tests {@code refersTo(null)}
 * instead of {@code get() != null}, so compiled code never loads the referent.
 *
 * Found by gc-common wave 5, lane D (2026-09-24). {@code WeakClearYoungProbe}
 * fails for two separate reasons, and neither is reference processing: with
 * {@code CRATONVM_DISABLE_JIT=1} it prints PROBE-OK on all three collectors.
 *
 * <ol>
 * <li>{@code get()}'s result is dead after the null test, but the compiled
 *     loop keeps it in a frame slot that the GC treats as a root, so the
 *     referent is strongly reachable at every allocation-driven pause (all
 *     three collectors). This probe never loads it, so that cause is gone.</li>
 * <li>G1 only: a conservatively-scanned JIT root pins its whole REGION out of
 *     the collection set, and every object in a pinned region survives the
 *     pause, reachable or not. The referents share a region with the live
 *     {@code WeakReference}s and the array a compiled frame holds, so that
 *     region is pinned at every young pause and the referents are never
 *     cleared. {@code CRATONVM_DBG_NO_JIT_ROOT_SCAN=1} (unsound, diagnostic
 *     only) makes G1 clear them.</li>
 * </ol>
 *
 * Measured on the round's w4 binary: HotSpot, Generational and ZGC print
 * PROBE-OK here; G1 prints PROBE-FAIL (cause 2). See
 * {@code docs/known-issues/gc/common-w4o-allocation-driven-collections-do-not-clear-weak-referents.md}.
 */
public class WeakClearNoLoadProbe {
    static volatile Object sink;

    static WeakReference<Object> make() {
        return new WeakReference<>(new Object());
    }

    @SuppressWarnings("unchecked")
    static WeakReference<Object>[] setup() {
        WeakReference<Object>[] r = new WeakReference[2];
        r[0] = new WeakReference<>(new Object());
        r[1] = make();
        return r;
    }

    static long spin(WeakReference<Object>[] r) {
        long n = 0;
        while ((!r[0].refersTo(null) || !r[1].refersTo(null)) && n < 5_000_000L) {
            sink = new byte[256];
            n += 2; // even: keep ShadowOddLongProbe's crash out of this probe
        }
        return n;
    }

    public static void main(String[] a) {
        WeakReference<Object>[] r = setup();
        long n = spin(r);
        boolean ok = r[0].refersTo(null) && r[1].refersTo(null);
        System.out.println("inline=" + (r[0].refersTo(null) ? "cleared" : "LIVE")
                + " viaMethod=" + (r[1].refersTo(null) ? "cleared" : "LIVE")
                + " allocations=" + (n / 2) + (ok ? " PROBE-OK" : " PROBE-FAIL"));
    }
}
