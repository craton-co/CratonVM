// TornadoVM equivalent of GpuDivChain.divChain — identical 48-step
// data-dependent integer-division chain so checksums are directly comparable.
// Usage: java @tornado-argfile --patch-module tornado.examples=. -m tornado.examples/uk.ac.manchester.tornado.examples.TornadoDivChain [n]
package uk.ac.manchester.tornado.examples;
import uk.ac.manchester.tornado.api.*;
import uk.ac.manchester.tornado.api.types.arrays.IntArray;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;

public class TornadoDivChain {
    // Must match GpuDivChain.divChain exactly (48 steps of x = x/d + 12345).
    private static void divChain(IntArray a, IntArray b, IntArray out) {
        for (@Parallel int i = 0; i < a.getSize(); i++) {
            int x = a.get(i);
            int d = b.get(i);
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
            out.set(i, x);
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 24);
        IntArray a = new IntArray(n);
        IntArray b = new IntArray(n);
        IntArray out = new IntArray(n);
        for (int i = 0; i < n; i++) {
            a.set(i, i * 1103515245 + 12345);
            b.set(i, 1 + (i % 13));
        }
        out.init(0);

        TaskGraph tg = new TaskGraph("s0")
            .transferToDevice(DataTransferMode.EVERY_EXECUTION, a, b)
            .task("t0", TornadoDivChain::divChain, a, b, out)
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

        long checksum = 0;
        for (int i = 0; i < n; i++) checksum += out.get(i);
        System.out.println("n=" + n);
        System.out.println("divchain_ms=" + best);
        System.out.println("DIV_CHECKSUM=" + checksum);
        System.out.println("OUT0=" + out.get(0) + " OUTN=" + out.get(n - 1));
    }
}
