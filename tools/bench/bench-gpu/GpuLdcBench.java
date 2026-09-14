// Same 96-step multiply-add chain shape as GpuWarm.heavy, but with constants
// chosen OUTSIDE sipush range (|c| > 32767): 1_000_003 and 777_777 instead of
// GpuWarm's 1103/12345. Both constants therefore compile to `ldc` (int
// constant-pool load) rather than `sipush`/`bipush`.
//
// Exercises the ldc-constants feature: per
// gpu-offload-followups-20260711.md item 6, `ldc`/`ldc_w`/
// `ldc2_w` are currently rejected by the analyzer, so today this kernel is
// INELIGIBLE (Rejected(Ldc) or similar) and always runs on CPU under both the
// plain and --gpu builds — warm_ms should be identical between them. Once ldc
// support lands, the kernel becomes a plain eligible void map (single counted
// loop, primitive int[] params, no calls) exactly like GpuWarm.heavy, and
// --gpu warm_ms should drop the same way GpuWarm's does.
//
// Usage: java GpuLdcBench [n] [reps]   (default n = 1<<26, reps = 5)
public class GpuLdcBench {
    static void heavyLdc(int[] a, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            int x = a[i];
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;  x = x * 1_000_003 + 777_777;
            out[i] = x;
        }
    }

    public static void main(String[] args) {
        int n    = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 26);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int[] a = new int[n]; int[] out = new int[n];
        for (int i = 0; i < n; i++) a[i] = i;
        heavyLdc(a, out);                                        // warmup (cold PTX, once eligible)
        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            heavyLdc(a, out);
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            if (ms < best) best = ms;
        }
        long sample = (long) out[0] + out[n/2] + out[n-1];       // cheap correctness sample
        System.out.println("kernel=ldc n=" + n + " warm_ms=" + best + " SAMPLE=" + sample);
    }
}
