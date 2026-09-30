// TornadoVM equivalent of GpuFloatDivChain.divChain — identical 64-step
// double-precision division chain so checksums are directly comparable.
// Usage: java @tornado-argfile --patch-module tornado.examples=. -m tornado.examples/uk.ac.manchester.tornado.examples.TornadoFloatDivChain [n]
package uk.ac.manchester.tornado.examples;
import uk.ac.manchester.tornado.api.*;
import uk.ac.manchester.tornado.api.types.arrays.DoubleArray;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;

public class TornadoFloatDivChain {
    // Must match GpuFloatDivChain.divChain exactly (64 steps of x = x/d + C).
    private static void divChain(DoubleArray a, DoubleArray b, DoubleArray out) {
        for (@Parallel int i = 0; i < a.getSize(); i++) {
            double x = a.get(i);
            double d = b.get(i);
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
            x = x / d + 1.0000001;  x = x / d + 1.0000001;  x = x / d + 1.0000001;
            x = x / d + 1.0000001;  x = x / d + 1.0000001;
            out.set(i, x);
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 22);
        DoubleArray a = new DoubleArray(n);
        DoubleArray b = new DoubleArray(n);
        DoubleArray out = new DoubleArray(n);
        for (int i = 0; i < n; i++) {
            a.set(i, 1.0 + (i % 1000) * 0.001);
            b.set(i, 1.01 + (i % 200) * 0.01);
        }
        out.init(0.0);

        TaskGraph tg = new TaskGraph("s0")
            .transferToDevice(DataTransferMode.EVERY_EXECUTION, a, b)
            .task("t0", TornadoFloatDivChain::divChain, a, b, out)
            .transferToHost(DataTransferMode.EVERY_EXECUTION, out);
        ImmutableTaskGraph itg = tg.snapshot();
        TornadoExecutionPlan plan = new TornadoExecutionPlan(itg);

        plan.execute();  // warmup: PTX compile + device alloc

        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) {
            long t0 = System.nanoTime();
            plan.execute();
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            if (ms < best) best = ms;
        }

        double checksum = 0;
        for (int i = 0; i < n; i++) checksum += out.get(i);
        System.out.println("n=" + n);
        System.out.println("fdivchain_ms=" + best);
        System.out.println("FDIV_CHECKSUM=" + checksum);
        System.out.println("OUT0=" + out.get(0) + " OUTN=" + out.get(n - 1));
    }
}
