// Where CratonVM's CPU path loses to HotSpot on the ray tracer kernel.
//
// The ray tracer record left "the CratonVM CPU path is still ~5.3x HotSpot
// on this kernel, and where the remaining 5.3x goes is unprofiled" as an
// open residual. A whole-kernel ratio cannot answer that: the kernel is
// ~40 float ops, 4 square roots, 12 divisions, a dozen selects and one
// int-packing store per pixel, and any one of them could carry the whole
// factor.
//
// This is the ablation. Each method isolates ONE construct the kernel
// actually contains, over the same arrays at the same length, with the
// same loop shape. Run the class on HotSpot and on CratonVM and the
// per-construct ratio says which one to go fix -- and, just as usefully,
// which ones are already at parity and would be wasted effort.
//
// Deliberately NOT annotated `@GpuKernel`: this measures the CPU path on
// both VMs. It takes no arguments beyond the size, so it needs no
// classpath but its own.
//
//   RayTracerCpuAblation [n] [iters]
public class RayTracerCpuAblation {

    // Each kernel is a separate static method so a JIT sees one shape per
    // compile, exactly as it does for `render`. All of them write a
    // distinct output array so nothing is dead code.

    /** Pure float multiply-add chain: the kernel's dot products. */
    static void mulAdd(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            float x = a[i], y = b[i];
            float s = x * 1.5f + y;
            s = s * 1.25f + x;
            s = s * 0.75f + y;
            s = s * 1.125f + x;
            s = s * 0.875f + y;
            s = s * 1.0625f + x;
            s = s * 0.9375f + y;
            s = s * 1.03125f + x;
            out[i] = s;
        }
    }

    /** `(float) Math.sqrt(...)` -- four per pixel in the real kernel. */
    static void sqrt(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            float x = a[i] + 1.0f, y = b[i] + 1.0f;
            out[i] = (float) Math.sqrt(x) + (float) Math.sqrt(y)
                   + (float) Math.sqrt(x + y) + (float) Math.sqrt(x * y + 1.0f);
        }
    }

    /** `Math.min(float,float)` -- the closest-hit selection. */
    static void min(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            float x = a[i], y = b[i];
            out[i] = Math.min(Math.min(x, y), Math.min(x + 1f, y + 1f))
                   + Math.min(Math.min(x + 2f, y + 2f), Math.min(x + 3f, y + 3f));
        }
    }

    /** A compare-and-select written as a ternary, no call involved. */
    static void select(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            float x = a[i], y = b[i];
            float s = x > y ? x : y;
            s += (x < y ? 1f : 0f);
            s += (x == y ? 1f : 0f);
            s += (s > 0f ? s : 0f);
            out[i] = s;
        }
    }

    /** Float division -- twelve per pixel in the real kernel's normals. */
    static void div(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            float x = a[i] + 1.0f, y = b[i] + 2.0f;
            out[i] = x / y + y / x + (x + y) / (x * y) + (x - y) / (x + y);
        }
    }

    /** The final pack: float to int, shifts, ors, one int store. */
    static void pack(float[] a, float[] b, int[] out) {
        for (int i = 0; i < out.length; i++) {
            float s = a[i] + b[i];
            s = s > 1f ? 1f : (s < 0f ? 0f : s);
            int g = (int) (s * 255f);
            out[i] = (g << 16) | (g << 8) | g;
        }
    }

    /** The memory floor: two reads and a write, no arithmetic. */
    static void copy(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] + b[i];
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2764800;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 12;

        float[] a = new float[n], b = new float[n], fout = new float[n];
        int[] iout = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = (i % 977) * 0.013f;
            b[i] = (i % 691) * 0.021f;
        }

        // Warm-up passes are separate from the timed ones so a tiering
        // decision is not inside the measurement.
        for (int w = 0; w < 3; w++) {
            copy(a, b, fout); mulAdd(a, b, fout); sqrt(a, b, fout);
            min(a, b, fout); select(a, b, fout); div(a, b, fout);
            pack(a, b, iout);
        }

        System.out.printf("ABLATION n=%d iters=%d%n", n, iters);
        report("copy   ", time(iters, () -> copy(a, b, fout)), n, fout, iout);
        report("mulAdd ", time(iters, () -> mulAdd(a, b, fout)), n, fout, iout);
        report("sqrt   ", time(iters, () -> sqrt(a, b, fout)), n, fout, iout);
        report("min    ", time(iters, () -> min(a, b, fout)), n, fout, iout);
        report("select ", time(iters, () -> select(a, b, fout)), n, fout, iout);
        report("div    ", time(iters, () -> div(a, b, fout)), n, fout, iout);
        report("pack   ", time(iters, () -> pack(a, b, iout)), n, fout, iout);
    }

    private static long time(int iters, Runnable r) {
        long best = Long.MAX_VALUE;
        for (int i = 0; i < iters; i++) {
            long t0 = System.nanoTime();
            r.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) best = dt;
        }
        return best;
    }

    // The checksum is printed so a run that optimised the loop away is
    // visible rather than merely fast.
    private static void report(String name, long bestNs, int n, float[] fout, int[] iout) {
        double cs = 0;
        for (int i = 0; i < n; i += 4096) cs += fout[i] + iout[i];
        System.out.printf("%s best_ms=%9.3f  ns_per_elem=%7.3f  checksum=%.4f%n",
                name, bestNs / 1e6, bestNs / (double) n, cs);
    }
}
