// Double-precision division chain — the floating-point analogue of
// GpuDivChain (integer). Designed to show a clear GPU win the same way the
// integer version does, but for a floating-point operation: x86 packed
// division (vdivpd/vdivps) is throughput-limited even under AVX2 — division
// execution units are shared and far less pipelined than multiply/add/FMA,
// unlike the "96 multiply-adds" bench where HotSpot's AVX2 auto-vectorizer
// is fully competitive with the GPU. A GPU has many more parallel lanes
// (thousands of threads vs 4-8 SIMD lanes) to amortize each division's cost.
//
// Eligible for CratonVM transparent offload with NO annotation hints: static
// void, primitive double[] params, canonical counted loop, plain `ddiv`
// (opcode 0x6F). The analyzer admits float/double division unconditionally
// (IEEE 754 has no trapping div-by-zero case, unlike integer division, so no
// AdmissionHint is needed the way frem/drem need AllowDivByZero).
//
// Numerically stable by construction: `x = x/d + C` with d > 1 is a
// contracting fixed-point iteration (x converges toward C*d/(d-1), a finite,
// well-conditioned value for the d range used here) — no overflow,
// underflow, or precision blowup across the chain regardless of n or reps.
//
// Usage: java GpuFloatDivChain [n] [reps]   (default n = 1<<22, reps = 5)
public class GpuFloatDivChain {
    static void divChain(double[] a, double[] b, double[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            double x = a[i];
            double d = b[i];
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;
            out[i] = x;
        }
    }

    public static void main(String[] args) {
        int n    = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 22);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        double[] a = new double[n];
        double[] b = new double[n];
        double[] out = new double[n];
        for (int i = 0; i < n; i++) {
            a[i] = 1.0 + (i % 1000) * 0.001;      // numerator, [1.0, 2.0)
            b[i] = 1.01 + (i % 200) * 0.01;        // divisor, [1.01, 3.0), never 0 or 1
        }
        divChain(a, b, out);                        // warmup (JIT / PTX compile)
        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            divChain(a, b, out);
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            if (ms < best) best = ms;
        }
        double checksum = 0;
        for (int i = 0; i < n; i++) checksum += out[i];
        System.out.println("n=" + n);
        System.out.println("fdivchain_ms=" + best);
        System.out.println("FDIV_CHECKSUM=" + checksum);
        System.out.println("OUT0=" + out[0] + " OUTN=" + out[n - 1]);
    }
}
