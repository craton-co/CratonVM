// TornadoVM equivalent of GpuDotBench.dotReduce — a genuine device-side
// @Reduce kernel (not a host-side sum), verified against the TornadoVM 4.0.1
// jars actually on this box: `javap -p -v` on
// tornado-examples-4.0.1-jdk25.jar's ReductionAddFloats.class shows the
// idiom TornadoVM's reduction compiler recognizes —
//   RuntimeVisibleParameterAnnotations: parameter 1: Luk/.../annotations/Reduce;
//   body: result.set(0, result.get(0) + <expr>)  inside a @Parallel loop
// — generalized here from one input array to two (a, b) for the dot product.
// The accumulator (`result`) is deliberately NOT included in
// transferToDevice: TornadoVM's reduction lowering seeds each kernel launch
// from the operator's identity element on-device (this is exactly what the
// shipped ReductionAddFloats example does — it never re-transfers `result`
// to the device across its 101 repeated plan.execute() calls), so no manual
// reset is needed between reps here either.
//
// Must match GpuDotBench.dotReduce's arithmetic exactly (sum of
// (long) a[i] * b[i]) so checksums are directly comparable.
// Usage: java @tornado-argfile --patch-module tornado.examples=. -m tornado.examples/uk.ac.manchester.tornado.examples.TornadoDotBench [n] [reps]
package uk.ac.manchester.tornado.examples;
import uk.ac.manchester.tornado.api.*;
import uk.ac.manchester.tornado.api.types.arrays.IntArray;
import uk.ac.manchester.tornado.api.types.arrays.LongArray;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.annotations.Reduce;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;

public class TornadoDotBench {
    // Must match GpuDotBench.dotReduce exactly (sum of (long)a[i]*(long)b[i]).
    private static void dotReduce(IntArray a, IntArray b, @Reduce LongArray result) {
        for (@Parallel int i = 0; i < a.getSize(); i++) {
            result.set(0, result.get(0) + (long) a.get(i) * (long) b.get(i));
        }
    }

    public static void main(String[] args) throws Exception {
        int n    = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 24);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        IntArray a = new IntArray(n);
        IntArray b = new IntArray(n);
        LongArray result = new LongArray(1);
        for (int i = 0; i < n; i++) {
            a.set(i, i * 1103515245 + 12345);
            b.set(i, 1 + (i % 13));
        }
        result.init(0);

        TaskGraph tg = new TaskGraph("s0")
            .transferToDevice(DataTransferMode.EVERY_EXECUTION, a, b)
            .task("t0", TornadoDotBench::dotReduce, a, b, result)
            .transferToHost(DataTransferMode.EVERY_EXECUTION, result);
        ImmutableTaskGraph itg = tg.snapshot();
        TornadoExecutionPlan plan = new TornadoExecutionPlan(itg);

        plan.execute();  // warmup: PTX compile + device alloc + reduction codegen

        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            plan.execute();
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            if (ms < best) best = ms;
        }

        long dot = result.get(0);
        System.out.println("n=" + n);
        System.out.println("dot_ms=" + best);
        System.out.println("DOT_CHECKSUM=" + dot);
    }
}
