// TornadoVM equivalent of GpuCompute.heavy -- must match its arithmetic
// exactly so checksums are directly comparable with GpuCompute output.
// AUDIT 2026-08-02: multiplier m = b[i] is now a per-element runtime value,
// not the compile-time constant 1103 -- see GpuCompute.java's header for why
// (a constant-coefficient affine chain gets folded to O(1) work by HotSpot's
// C2, so it no longer measures genuine per-element compute).
// Usage: java @tornado-argfile --patch-module tornado.examples=. -m tornado.examples/uk.ac.manchester.tornado.examples.TornadoGpuCompute [n]
package uk.ac.manchester.tornado.examples;
import uk.ac.manchester.tornado.api.*;
import uk.ac.manchester.tornado.api.types.arrays.IntArray;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;

public class TornadoGpuCompute {
    // Must match GpuCompute.heavy exactly (128 iterations of x = x*m+12345, m = b[i])
    private static void heavy(IntArray a, IntArray b, IntArray out) {
        for (@Parallel int i = 0; i < a.getSize(); i++) {
            int m = b.get(i);
            int x = a.get(i);
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
            out.set(i, x);
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 20);
        IntArray a = new IntArray(n);
        IntArray b = new IntArray(n);
        IntArray out = new IntArray(n);
        for (int i = 0; i < n; i++) { a.set(i, i); b.set(i, 1 + (i % 13)); }
        out.init(0);

        TaskGraph tg = new TaskGraph("s0")
            .transferToDevice(DataTransferMode.EVERY_EXECUTION, a, b)
            .task("t0", TornadoGpuCompute::heavy, a, b, out)
            .transferToHost(DataTransferMode.EVERY_EXECUTION, out);
        ImmutableTaskGraph itg = tg.snapshot();
        TornadoExecutionPlan plan = new TornadoExecutionPlan(itg);

        // warmup: compile PTX kernel and allocate GPU buffers
        plan.execute();

        long t0 = System.nanoTime();
        plan.execute();
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        long checksum = 0;
        for (int i = 0; i < n; i++) checksum += out.get(i);
        System.out.println("n=" + n);
        System.out.println("heavy_ms=" + ms);
        System.out.println("COMPUTE_CHECKSUM=" + checksum);
        System.out.println("OUT0=" + out.get(0) + " OUTN=" + out.get(n - 1));
    }
}
