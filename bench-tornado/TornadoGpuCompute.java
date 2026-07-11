// TornadoVM equivalent of GpuCompute.heavy — identical 96x multiply-add chain
// so checksums are directly comparable with GpuCompute output.
// Usage: java @tornado-argfile --patch-module tornado.examples=. -m tornado.examples/uk.ac.manchester.tornado.examples.TornadoGpuCompute [n]
package uk.ac.manchester.tornado.examples;
import uk.ac.manchester.tornado.api.*;
import uk.ac.manchester.tornado.api.types.arrays.IntArray;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;

public class TornadoGpuCompute {
    // Must match GpuCompute.heavy exactly (96 iterations of x = x*1103+12345)
    private static void heavy(IntArray a, IntArray out) {
        for (@Parallel int i = 0; i < a.getSize(); i++) {
            int x = a.get(i);
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            x = x * 1103 + 12345; x = x * 1103 + 12345; x = x * 1103 + 12345;
            out.set(i, x);
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 20);
        IntArray a = new IntArray(n);
        IntArray out = new IntArray(n);
        for (int i = 0; i < n; i++) a.set(i, i);
        out.init(0);

        TaskGraph tg = new TaskGraph("s0")
            .transferToDevice(DataTransferMode.EVERY_EXECUTION, a)
            .task("t0", TornadoGpuCompute::heavy, a, out)
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
