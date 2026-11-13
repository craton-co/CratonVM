// Data-dependent integer-division chain — the "GPU >> any CPU" concept prover.
//
// Unlike GpuCompute.heavy (a multiply-add chain that HotSpot C2 auto-vectorizes
// with AVX2, making a strong CPU baseline), x86 has NO SIMD integer division
// and the divisor here is data-dependent (b[i], never a compile-time constant),
// so strength-reduction to multiply-shift is impossible. Every CPU — HotSpot
// included — must execute 48 serial, dependent ~20-30-cycle scalar idivs per
// element. The GPU emulates idiv too (~20 instructions on sm_75), but runs
// tens of thousands of elements in parallel.
//
// Eligible for CratonVM transparent offload: static void, primitive int arrays,
// canonical counted loop, sipush-range constants, no calls/allocation/fields.
// b[i] is always >= 1 by construction so the div-by-zero deopt guard never fires.
//
// Usage: java GpuDivChain [n] [reps]   (default n = 1<<24, reps = 5)
public class GpuDivChain {
    static void divChain(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            int x = a[i];
            int d = b[i];
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            x = x / d + 12345;  x = x / d + 12345;  x = x / d + 12345;
            out[i] = x;
        }
    }

    public static void main(String[] args) {
        int n    = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 24);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i * 1103515245 + 12345;      // spread inputs (LCG-style)
            b[i] = 1 + (i % 13);                // divisor in [1,13], never 0
        }
        divChain(a, b, out);                     // warmup (JIT / PTX compile)
        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            divChain(a, b, out);
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            if (ms < best) best = ms;
        }
        long checksum = 0;
        for (int i = 0; i < n; i++) checksum += out[i];
        System.out.println("n=" + n);
        System.out.println("divchain_ms=" + best);
        System.out.println("DIV_CHECKSUM=" + checksum);
        System.out.println("OUT0=" + out[0] + " OUTN=" + out[n - 1]);
    }
}
