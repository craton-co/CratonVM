import java.lang.ref.SoftReference;
import java.lang.reflect.Field;

/**
 * zr z1/rf (2026-10-12): the SoftReference LRU policy on ZGC against HotSpot 25
 * {@code -XX:+UseZGC} (the ZGC half of
 * {@code gch-h2jr-soft-lru-window-on-g1-and-zgc-diverges-from-hotspot-FIXED-20261012}).
 *
 * <p>Run every mode with {@code -Xmx64m -XX:SoftRefLRUPolicyMSPerMB=20}: the
 * policy then keeps a soft referent idle for up to about free-MiB x 20 ms
 * (~1.2 s with ~60 MiB free).
 *
 * <ul>
 *   <li>{@code gc}: {@code hot} is read with {@code get()} every 2 ms, {@code idle}
 *       never; a {@code System.gc()} every 100 ms for 4 s. HotSpot keeps
 *       {@code hot} and clears {@code idle}. A VM whose soft clock never advances
 *       ({@code SoftReference.clock} stays 0, so a bytecode {@code get()} never
 *       refreshes the row) clears {@code hot} as well.</li>
 *   <li>{@code alloc}: the same two references, but the collections are the
 *       allocation-triggered ones (a 1 MiB {@code byte[]} churn, no
 *       {@code System.gc()}) for 4 s, so the door is the collector's own trigger
 *       rather than {@code System.gc()}. Same expected lines.</li>
 *   <li>{@code clock}: does {@code SoftReference.clock} advance across
 *       collections? Needs {@code --add-opens java.base/java.lang.ref=ALL-UNNAMED}
 *       on HotSpot; prints {@code clock advanced: true} there (the clock is the
 *       time of the last collection's reference processing), or
 *       {@code clock: inaccessible} when reflection is refused.</li>
 * </ul>
 *
 * <p>stdout is deterministic; stderr carries the millisecond at which {@code hot}
 * was lost (or -1), a diagnostic only.
 */
public class Zr1RfSoftLruProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "gc";
        switch (mode) {
            case "gc" -> lru(false);
            case "alloc" -> lru(true);
            case "clock" -> clock();
            default -> {
                System.out.println("unknown mode " + mode);
                System.exit(2);
            }
        }
        System.out.println("done " + mode);
    }

    static void lru(boolean byAllocation) throws InterruptedException {
        SoftReference<Object> hot = new SoftReference<>(new Object());
        SoftReference<Object> idle = new SoftReference<>(new Object());
        long start = System.nanoTime();
        long lastGc = start;
        long hotLost = -1;
        while (System.nanoTime() - start < 4_000_000_000L) {
            Object h = hot.get();
            if (h == null && hotLost < 0) {
                hotLost = (System.nanoTime() - start) / 1_000_000L;
            }
            sink = h;
            sink = null;
            if (byAllocation) {
                // ~0.5 GiB/s of short-lived garbage: the collector's own
                // trigger runs many cycles a second at -Xmx64m.
                for (int i = 0; i < 1; i++) {
                    sink = new byte[1 << 20];
                }
                sink = null;
            } else {
                long now = System.nanoTime();
                if (now - lastGc >= 100_000_000L) {
                    System.gc();
                    lastGc = System.nanoTime();
                }
            }
            Thread.sleep(2);
        }
        System.err.println("hot lost at ms=" + hotLost);
        String tag = byAllocation ? "alloc" : "gc";
        System.out.println(tag + " hot kept: " + (hot.get() != null && hotLost < 0));
        System.out.println(tag + " idle cleared: " + (idle.get() == null));
    }

    static void clock() throws Exception {
        Field f;
        try {
            f = SoftReference.class.getDeclaredField("clock");
            f.setAccessible(true);
        } catch (RuntimeException | NoSuchFieldException e) {
            System.out.println("clock: inaccessible");
            return;
        }
        SoftReference<Object> r = new SoftReference<>(new Object());
        System.gc();
        long c1 = f.getLong(null);
        Thread.sleep(50);
        System.gc();
        long c2 = f.getLong(null);
        sink = r.get();
        System.out.println("clock advanced: " + (c2 > c1 && c1 > 0));
    }
}
