import java.util.concurrent.CyclicBarrier;

/**
 * Isolates the write-barrier cost per store shape, with CONSTANT TOTAL WORK:
 * `ops` stores in total, split evenly over `threads`. No allocation and no GC
 * inside the timed loop; every array is pre-allocated and pre-filled.
 *
 * Usage: BarrierProbe <mode> <threads> <ops> <arraylen> [shared|perthread]
 *
 *   primstore  int[i]     = k                 no GC barrier of any kind
 *   refnull    Object[i]  = null              barrier's null arm
 *   refread    reads Object[i], no store      the read path only
 *   refself    Object[i]  = theArray          non-null, SAME region as dst
 *   refstore   Object[i]  = other[i]          non-null, source region varies
 *   pairnull   two null stores to one slot
 *   pairref    a null store then a non-null one to one slot
 *
 * `arraylen` must keep Object[len] under G1's humongous threshold
 * (region_size / 2); 4096 elements is safe for a 1 MiB region.
 *
 * The fifth argument decides whether the threads share ONE set of arrays
 * (allocated on the main thread, which is what the original 2026-08-17
 * measurement did — its `same_region_skipped=1839949 / 2M` census only comes
 * out that way if every referent was allocated in one contiguous run) or each
 * get their own. `shared` also puts every thread's stores into ONE destination
 * region, which is the case the remembered set contends on; `perthread` is the
 * control that separates Java-level false sharing from collector cost.
 */
public class BarrierProbe {

    static volatile long sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "refstore";
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        int ops = args.length > 2 ? Integer.parseInt(args[2]) : 2_000_000;
        int len = args.length > 3 ? Integer.parseInt(args[3]) : 4096;
        boolean shared = args.length <= 4 || args[4].equals("shared");

        final int per = ops / threads;
        final int mask = len - 1;
        if ((len & mask) != 0) throw new IllegalArgumentException("arraylen must be a power of two");

        final Object[] sDst = new Object[len];
        final Object[] sOther = new Object[len];
        final int[] sPrim = new int[len];
        for (int i = 0; i < len; i++) { sDst[i] = new Object(); sOther[i] = new Object(); }

        final CyclicBarrier start = new CyclicBarrier(threads + 1);
        final CyclicBarrier end = new CyclicBarrier(threads + 1);
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            ts[t] = new Thread(() -> {
                Object[] dst = sDst, other = sOther;
                int[] prim = sPrim;
                if (!shared) {
                    dst = new Object[len];
                    other = new Object[len];
                    prim = new int[len];
                    for (int i = 0; i < len; i++) { dst[i] = new Object(); other[i] = new Object(); }
                }
                long local = 0;
                try { start.await(); } catch (Exception e) { throw new RuntimeException(e); }
                switch (mode) {
                    case "primstore": for (int i = 0; i < per; i++) { prim[i & mask] = i; } break;
                    case "refnull":   for (int i = 0; i < per; i++) { dst[i & mask] = null; } break;
                    case "refread":   for (int i = 0; i < per; i++) { if (dst[i & mask] != null) local++; } break;
                    case "refself":   for (int i = 0; i < per; i++) { dst[i & mask] = dst; } break;
                    case "refstore":  for (int i = 0; i < per; i++) { dst[i & mask] = other[i & mask]; } break;
                    case "pairnull":  for (int i = 0; i < per; i++) { dst[i & mask] = null; dst[i & mask] = null; } break;
                    case "pairref":   for (int i = 0; i < per; i++) { dst[i & mask] = null; dst[i & mask] = other[i & mask]; } break;
                    default: throw new IllegalArgumentException(mode);
                }
                sink += local + (dst[0] == null ? 0 : 1) + prim[0];
                try { end.await(); } catch (Exception e) { throw new RuntimeException(e); }
            }, "bp-" + t);
            ts[t].setDaemon(true);
            ts[t].start();
        }
        start.await();
        long t0 = System.nanoTime();
        end.await();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        for (Thread th : ts) th.join();
        System.out.printf("BARRIER mode=%-9s threads=%d ops=%d len=%d arrays=%s ms=%d ns/op=%.1f sink=%d%n",
                mode, threads, ops, len, shared ? "shared" : "perthread", ms, (ms * 1e6) / ops, sink);
    }
}
