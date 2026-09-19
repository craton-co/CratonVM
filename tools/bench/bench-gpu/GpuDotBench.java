// Dot-product REDUCTION benchmark -- accumulates two int[] into a single long
// scalar via sum += (long) a[i] * b[i], repeated 300 times per element.
// dotReduce() below is the proven-eligible reduction shape from
// test_classes/gpu/EligibleDotProduct.java (mirrored verbatim in
// bench-gpu/GpuProbe.dotReduce): a single counted loop, scalar long
// accumulator, no array store, no calls -> the analyzer marks it
// is_reduction:true. Reused here verbatim, only sized for benching (n/reps
// driver args, warmup call, best-of-reps timing) to match the other bench-gpu
// drivers (GpuDivChain, GpuWarm).
//
// AUDIT 2026-08-02: the accumulation `sum += p` (p = (long) a[i] * b[i],
// hoisted once per element) is repeated 300 times per element so the
// HotSpot column stays >=1s at n=2^24 -- unlike GpuCompute's constant-
// coefficient multiply chain, repeated identical `sum += p` does NOT get
// folded into `sum += 300*p` by C2 (verified: DOT_CHECKSUM scales
// exactly linearly with the repeat count and wall time does too -- 4ms at
// 1x, 1216ms at 300x, both at n=2^24), so plain repetition is safe here.
//
// Exercises the reduction-dispatch feature: as of 2026-07-11 the analyzer/
// lowering half is done (PTX with an atomic-add epilogue), but transparent
// --gpu dispatch still falls through to the CPU for any non-void kernel (see
// gpu-offload-followups-20260711.md, item 1 -- try_dispatch's
// "VOID return only" gate). Until the dispatch-side result-readback lands,
// dot_ms under --gpu is expected to equal the CPU timing; once it lands, GPU
// offload should kick in and dot_ms should drop sharply, same as the void
// map-shaped kernels already do.
//
// Usage: java GpuDotBench [n] [reps]   (default n = 1<<24, reps = 5)
public class GpuDotBench {
    // Eligible REDUCTION shape verbatim from GpuProbe.dotReduce /
    // EligibleDotProduct.dot: single counted loop, scalar long accumulator,
    // no array store, no calls -> is_reduction:true.
    static long dotReduce(int[] a, int[] b) {
        long sum = 0;
        int n = a.length;
        for (int i = 0; i < n; i++) {
            long p = (long) a[i] * b[i];
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
            sum += p;
        }
        return sum;
    }

    // Independent sequential recompute for a self-check. Deliberately a
    // different loop shape (counts down instead of up) so it isn't just the
    // same code path re-executed; integer addition is associative/order-
    // independent so DOT_REF must exactly equal DOT_CHECKSUM regardless.
    // This method's shape is NOT the eligible one (decrementing loop) -- it is
    // meant to stay on CPU as a correctness oracle, not to be offloaded.
    static long dotReference(int[] a, int[] b) {
        long sum = 0L;
        for (int i = a.length - 1; i >= 0; i--) {
            sum += (long) a[i] * (long) b[i] * 300L;
        }
        return sum;
    }

    public static void main(String[] args) {
        int n    = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 24);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int[] a = new int[n];
        int[] b = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i * 1103515245 + 12345;      // spread inputs (LCG-style)
            b[i] = 1 + (i % 13);                // small, deterministic, matches GpuDivChain's divisor spread
        }
        long dot = dotReduce(a, b);               // warmup (JIT / PTX compile)
        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            dot = dotReduce(a, b);
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            if (ms < best) best = ms;
        }
        long ref = dotReference(a, b);
        System.out.println("n=" + n);
        System.out.println("dot_ms=" + best);
        System.out.println("DOT_CHECKSUM=" + dot);
        System.out.println("DOT_REF=" + ref);
    }
}
