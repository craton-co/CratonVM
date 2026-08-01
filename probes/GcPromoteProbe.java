// Probe for HIB-GCOVERHEAD-HALFFULL.1.
//
// Builds a long-lived object set larger than the young semi-space, doing every
// allocation from inside a HOT (JIT-compiled) method so that each forced GC
// sees a live compiled frame. On a healthy VM the survivors drain young (either
// by the moving Cheney cycle or by the non-moving sweep's selective promotion)
// and the run completes. On the broken VM the young generation can never drain,
// every forced GC frees ~nothing, the GC-overhead streak latches, and the run
// dies with OutOfMemoryError while the heap is roughly half empty.
//
// Usage (this sizing makes the live set comfortably exceed the 32 MB young semi
// that -Xmx 128m produces):
//
//   javac -d . probes/GcPromoteProbe.java
//   cratonvm --java-home "$JDK" --Xmx 128m -cp . GcPromoteProbe 300000 512 16
//
// Add CRATONVM_DBG_GC_OVERHEAD=1 to watch the productivity accounting. The
// discriminator is `promoted=`: a healthy run promotes megabytes per cycle, a
// broken one reports 0 forever. Reference numbers, 2026-07-31:
//
//   HotSpot                                       0.14 s
//   CratonVM, fixed                               3.4 s
//   CratonVM, broken                              wedged, no progress past 20 MB
//   CratonVM, broken + CRATONVM_NO_MOVING_YOUNG=1 3.9 s — the opt-out
//       short-circuits before the coverage flag is set, so promotion survives.
//       That arm is what identified the fault.
public class GcPromoteProbe {
    static Object[] retained;
    static Object junk;
    static long sink;

    // Hot: called once per chunk, so it crosses the JIT threshold quickly and
    // every allocation below happens with a compiled frame on the stack.
    static int fill(int from, int to, int churn) {
        for (int i = from; i < to; i++) {
            retained[i] = new byte[128];
            for (int j = 0; j < churn; j++) {
                junk = new byte[64];
                sink += ((byte[]) junk).length;
            }
        }
        return to;
    }

    public static void main(String[] args) throws Exception {
        int keep = Integer.parseInt(args[0]);   // retained objects (128 B each)
        int chunk = args.length > 1 ? Integer.parseInt(args[1]) : 512;
        int churn = args.length > 2 ? Integer.parseInt(args[2]) : 16;
        retained = new Object[keep];
        long t0 = System.currentTimeMillis();
        int i = 0;
        while (i < keep) {
            int to = Math.min(i + chunk, keep);
            i = fill(i, to, churn);
            if ((i % (chunk * 64)) == 0) {
                Runtime r = Runtime.getRuntime();
                System.out.println("kept=" + i
                        + " retainedMB=" + (((long) i * 128) >> 20)
                        + " total=" + (r.totalMemory() >> 20)
                        + "M free=" + (r.freeMemory() >> 20)
                        + "M t=" + (System.currentTimeMillis() - t0) + "ms");
            }
        }
        // Keep the set reachable to the very end.
        int nonNull = 0;
        for (int k = 0; k < keep; k++) {
            if (retained[k] != null) {
                nonNull++;
            }
        }
        System.out.println("OK kept=" + nonNull + " sink=" + sink
                + " elapsed=" + (System.currentTimeMillis() - t0) + "ms");
    }
}
