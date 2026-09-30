// TornadoVM equivalent of GpuProbe.vaddMap — same init so checksums match.
// Usage: java @tornado-argfile --patch-module tornado.examples=. -m tornado.examples/uk.ac.manchester.tornado.examples.TornadoVadd [n]
package uk.ac.manchester.tornado.examples;
import uk.ac.manchester.tornado.api.*;
import uk.ac.manchester.tornado.api.types.arrays.IntArray;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;

public class TornadoVadd {
    // Matches GpuProbe.vaddMap: out[i] = a[i] + b[i]
    private static void vaddMap(IntArray a, IntArray b, IntArray out) {
        for (@Parallel int i = 0; i < a.getSize(); i++) {
            out.set(i, a.get(i) + b.get(i));
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 20);
        IntArray a = new IntArray(n);
        IntArray b = new IntArray(n);
        IntArray out = new IntArray(n);
        // Same initialization as GpuProbe: a[i]=i, b[i]=(i*7)%1000
        for (int i = 0; i < n; i++) {
            a.set(i, i);
            b.set(i, (i * 7) % 1000);
        }
        out.init(0);

        TaskGraph tg = new TaskGraph("s0")
            .transferToDevice(DataTransferMode.EVERY_EXECUTION, a, b)
            .task("t0", TornadoVadd::vaddMap, a, b, out)
            .transferToHost(DataTransferMode.EVERY_EXECUTION, out);
        ImmutableTaskGraph itg = tg.snapshot();
        TornadoExecutionPlan plan = new TornadoExecutionPlan(itg);

        // warmup
        plan.execute();

        long t0 = System.nanoTime();
        plan.execute();
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        long mapChecksum = 0;
        for (int i = 0; i < n; i++) mapChecksum += out.get(i);
        System.out.println("n=" + n);
        System.out.println("MAP_CHECKSUM=" + mapChecksum);
        System.out.println("vadd_ms=" + ms);
        System.out.println("OUT0=" + out.get(0) + " OUTN=" + out.get(n - 1));
    }
}
