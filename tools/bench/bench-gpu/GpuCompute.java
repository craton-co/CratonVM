// Compute-heavy ELIGIBLE kernel for the gpu-offload suite. A single counted
// loop (one GPU thread per element) whose body is a long, data-dependent
// integer multiply-add chain: x = x*m+12345 repeated 128 times, where
// m = b[i] is a per-element RUNTIME value (not a compile-time constant).
//
// AUDIT 2026-08-02: an earlier revision used a compile-time-constant
// multiplier (x = x*1103+12345) repeated N times. That is an affine
// recurrence with constant coefficients, and composition of affine maps
// with constant coefficients is itself affine - C2's GVN/reassociation
// folds the whole chain down to a single multiply+add (closed-form
// coefficients 1103^N and the corresponding geometric-series constant),
// regardless of how many lines are unrolled in source. Measured effect:
// unrolling from 96 to 6528 lines changed HotSpot's time by less than 2x
// (near-noise), because both were doing ~O(1) work per element after
// folding - the benchmark was silently memory-bandwidth-bound, not
// compute-bound. Sourcing the multiplier from a second array (like
// GpuDivChain's divisor) makes each step depend on a value only known at
// runtime, so no closed form exists and every step is genuinely executed
// (verified: 96 data-dependent steps measured ~888ms at n=2^24, 128
// steps ~1273ms, both roughly linear in step count, vs ~16ms for the old
// 96-step constant-coefficient version). Still eligible for CratonVM
// transparent offload: static void, primitive int arrays, canonical
// counted loop, sipush-range constant, no calls/allocation/inner loop.
// Usage: java GpuCompute [n]   (default n = 1<<20)
public class GpuCompute {
    static void heavy(int[] a, int[] b, int[] out) {
        int x = 0;
        int n = a.length;
        for (int i = 0; i < n; i++) {
            int m = b[i];
            x = a[i];
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            x = x * m + 12345;
            out[i] = x;
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 20);
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
            b[i] = 1 + (i % 13);
        }
        long t0 = System.nanoTime();
        heavy(a, b, out);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        long checksum = 0;
        for (int i = 0; i < n; i++) {
            checksum += out[i];
        }
        System.out.println("n=" + n);
        System.out.println("heavy_ms=" + ms);
        System.out.println("COMPUTE_CHECKSUM=" + checksum);
        System.out.println("OUT0=" + out[0] + " OUTN=" + out[n - 1]);
    }
}
