import java.lang.ref.WeakReference;

/**
 * An allocation-driven collection must clear a WeakReference whose referent is
 * otherwise unreachable, as HotSpot's young collections do.
 *
 * Found by the gc-common round (2026-09-24). `inline` is created in `main`;
 * `viaMethod` is created in a callee, so no stale slot of `main` can hold its
 * referent. HotSpot clears both after ~170k allocations on every collector.
 * CratonVM (pre-round binary too): G1 and ZGC keep both alive for 5M
 * allocations, Generational keeps `inline`; only `System.gc()` clears them. See
 * `docs/known-issues/gc/common-w4o-allocation-driven-collections-do-not-clear-weak-referents.md`.
 *
 * Prints `PROBE-OK` when both clear without System.gc().
 */
public class WeakClearYoungProbe {
    static volatile Object sink;

    static WeakReference<Object> make() {
        return new WeakReference<>(new Object());
    }

    public static void main(String[] a) {
        WeakReference<Object> inline = new WeakReference<>(new Object());
        WeakReference<Object> viaMethod = make();
        long n = 0;
        while ((inline.get() != null || viaMethod.get() != null) && n < 5_000_000L) {
            sink = new byte[256];
            n += 2; // even: keep ShadowOddLongProbe's crash out of this probe
        }
        boolean ok = inline.get() == null && viaMethod.get() == null;
        System.out.println("inline=" + (inline.get() == null ? "cleared" : "LIVE")
                + " viaMethod=" + (viaMethod.get() == null ? "cleared" : "LIVE")
                + " allocations=" + (n / 2) + (ok ? " PROBE-OK" : " PROBE-FAIL"));
    }
}
