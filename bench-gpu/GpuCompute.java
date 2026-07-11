// Compute-heavy ELIGIBLE kernel for the gpu-offload suite. A single counted
// loop (one GPU thread per element) whose body is a long, data-dependent
// integer multiply-add chain. Constants stay in sipush range (<=32767) so the
// analyzer admits them (large ints need `ldc`, which it rejects). No method
// calls / allocation / inner loop => eligible. COMPUTE-bound (heavy arithmetic,
// tiny memory traffic), so the GPU's thousands of cores crush serial CPU exec:
// ~seconds on GPU vs ~100s on CratonVM's CPU. void map => offloads under --gpu.
// Usage: java GpuCompute [n]   (default n = 1<<20)
public class GpuCompute {
    static void heavy(int[] a, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            int x = a[i];
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            x = x * 1103 + 12345;
            out[i] = x;
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 20);
        int[] a = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
        }
        long t0 = System.nanoTime();
        heavy(a, out);
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
