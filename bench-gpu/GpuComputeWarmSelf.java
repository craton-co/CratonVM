// Same kernel as GpuCompute.heavy, but declared in the class that also owns
// main(), to test whether the offload gate only analyses the entry class.
public class GpuComputeWarmSelf {
    static void heavy(int[] a, int[] b, int[] out) {
        for (int i = 0; i < a.length; i++) {
            int x = a[i];
            int m = b[i];
            for (int k = 0; k < 128; k++) x = x * m + 12345;
            out[i] = x;
        }
    }
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 24);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int[] a = new int[n], b = new int[n], out = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = 1 + (i % 13); }
        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            heavy(a, b, out);
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            if (ms < best) best = ms;
        }
        long checksum = 0;
        for (int i = 0; i < n; i++) checksum += out[i];
        System.out.println("heavy_ms=" + best);
        System.out.println("COMPUTE_CHECKSUM=" + checksum);
    }
}
