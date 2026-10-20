import java.lang.ref.PhantomReference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;

/**
 * gch h1/ak (2026-10-08): reference-processing parity rows.
 *
 * <ul>
 *   <li>{@code softlru}: the SoftReference LRU window
 *       ({@code gcg-g5rs-shadowed-softreference-get-never-touches-the-lru-row}).
 *       Two soft references to fresh objects: {@code hot} is read with
 *       {@code get()} every 2 ms, {@code idle} never after construction. A
 *       {@code System.gc()} runs every 100 ms for about 4 s. Run with
 *       {@code -Xmx64m -XX:SoftRefLRUPolicyMSPerMB=20}: HotSpot 25 Serial's
 *       threshold is then about free-MiB x 20 ms (~1.2 s), so {@code hot}
 *       (idle for one GC interval) is kept and {@code idle} is cleared. A VM
 *       whose processor row never sees {@code get()} clears {@code hot} too;
 *       a VM that never clears soft references keeps {@code idle}.</li>
 *   <li>{@code softidle}: the free-heap figure of the soft LRU policy. One
 *       soft reference left idle for about 800 ms while a {@code System.gc()}
 *       runs every 100 ms. Run with {@code -Xmx64m
 *       -XX:SoftRefLRUPolicyMSPerMB=20}: HotSpot 25 Serial's
 *       {@code LRUMaxHeapPolicy} allows (max capacity - used at the last GC)
 *       ~ 60 MiB x 20 ms ~ 1.2 s of idleness, so it keeps the referent. A VM
 *       that feeds the policy the free space of the young generation alone
 *       (~20 MiB, ~0.4 s) clears it.</li>
 *   <li>{@code refs-thread}: {@code GcgX3XlogProbe refs} after one
 *       application thread has been started and joined. Compared under
 *       {@code -Xlog:gc+phases+ref=debug}: HotSpot counts
 *       {@code WeakReference Discovered: 1}; a VM whose first
 *       {@code Thread.start} initialises {@code java.util.logging} (the
 *       launcher's logging pre-warm) discovers the pre-warm's dead
 *       {@code LogManager$LoggerWeakRef}s as well.</li>
 *   <li>{@code softcache} (gch h2/jr): a soft-valued cache churned far past
 *       {@code -Xmx64m}; must print {@code softcache oome: false} (the risk
 *       gate of {@code CRATONVM_SOFTREF_HOTSPOT_FREE}, see the method).</li>
 * </ul>
 *
 * stdout is deterministic; only {@code softcache} prints on stderr (its hit
 * count, a diagnostic).
 */
public class GchAk1RefProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "softlru";
        switch (mode) {
            case "softlru":
                softLru();
                break;
            case "softidle":
                softIdle();
                break;
            case "refs-thread":
                refsAfterThread();
                break;
            case "softcache":
                softCache();
                break;
            default:
                System.out.println("unknown mode " + mode);
                System.exit(2);
        }
        System.out.println("done " + mode);
    }

    static void softLru() throws InterruptedException {
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
            long now = System.nanoTime();
            if (now - lastGc >= 100_000_000L) {
                System.gc();
                lastGc = System.nanoTime();
            }
            Thread.sleep(2);
        }
        System.gc();
        System.out.println("softlru hot kept: " + (hot.get() != null && hotLost < 0));
        System.out.println("softlru idle cleared: " + (idle.get() == null));
    }

    static void softIdle() throws InterruptedException {
        // The constructor copies the previous GC clock, not its wall time.
        // Establish that clock so VM startup cannot consume the 1.2 s window
        // before this 800 ms test begins. SoftLruClockAgeProbe covers that age.
        System.gc();
        SoftReference<Object> idle = new SoftReference<>(new Object());
        long start = System.nanoTime();
        long lastGc = start;
        while (System.nanoTime() - start < 800_000_000L) {
            long now = System.nanoTime();
            if (now - lastGc >= 100_000_000L) {
                System.gc();
                lastGc = System.nanoTime();
            }
            Thread.sleep(2);
        }
        System.gc();
        System.out.println("softidle kept: " + (idle.get() != null));
    }

    static void refsAfterThread() throws InterruptedException {
        Thread t = new Thread(() -> sink = new Object(), "gch-ak1-worker");
        t.start();
        t.join();
        sink = null;
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        WeakReference<Object> w = new WeakReference<>(new Object(), q);
        Object softReferent = new Object();
        SoftReference<Object> s = new SoftReference<>(softReferent);
        PhantomReference<Object> p = new PhantomReference<>(new Object(), q);
        softReferent = null;
        System.gc();
        sink = w;
        sink = s;
        sink = p;
        sink = null;
    }

    /**
     * gch h2/jr: the risk gate of {@code CRATONVM_SOFTREF_HOTSPOT_FREE}. A
     * soft-valued cache of 256 KiB entries grows by 2000 entries (~500 MiB in
     * all) under {@code -Xmx64m}; the newest eight entries are read back every
     * round (a hot cache tail), and cleared entries are pruned every 64
     * rounds. The JDK contract (soft references are cleared before an
     * {@code OutOfMemoryError}) and HotSpot 25 Serial both answer
     * {@code softcache oome: false}. A free figure that over-reports what the
     * young generation can still take would keep idle entries until the
     * allocation fails.
     */
    static void softCache() {
        java.util.ArrayList<SoftReference<byte[]>> cache = new java.util.ArrayList<>();
        long hits = 0;
        boolean oome = false;
        try {
            for (int i = 0; i < 2000; i++) {
                cache.add(new SoftReference<>(new byte[256 * 1024]));
                for (int j = Math.max(0, cache.size() - 8); j < cache.size(); j++) {
                    if (cache.get(j).get() != null) {
                        hits++;
                    }
                }
                if ((i & 63) == 63) {
                    cache.removeIf(r -> r.get() == null);
                }
            }
        } catch (OutOfMemoryError e) {
            cache = null;
            oome = true;
        }
        sink = cache;
        sink = null;
        System.out.println("softcache oome: " + oome);
        System.err.println("[softcache] hits=" + hits);
    }
}
