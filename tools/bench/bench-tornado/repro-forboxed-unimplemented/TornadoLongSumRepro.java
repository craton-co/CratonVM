// Isolating repro (works): single-array LongArray -> LongArray reduce, no
// widening cast. See README.md in this directory.
package uk.ac.manchester.tornado.examples;
import uk.ac.manchester.tornado.api.*;
import uk.ac.manchester.tornado.api.types.arrays.LongArray;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.annotations.Reduce;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;

public class TornadoLongSumRepro {
    private static void sumReduce(LongArray a, @Reduce LongArray result) {
        for (@Parallel int i = 0; i < a.getSize(); i++) {
            result.set(0, result.get(0) + a.get(i));
        }
    }

    public static void main(String[] args) throws Exception {
        int n = 1024;
        LongArray a = new LongArray(n);
        LongArray result = new LongArray(1);
        for (int i = 0; i < n; i++) a.set(i, i);
        result.init(0);

        TaskGraph tg = new TaskGraph("s0")
            .transferToDevice(DataTransferMode.EVERY_EXECUTION, a)
            .task("t0", TornadoLongSumRepro::sumReduce, a, result)
            .transferToHost(DataTransferMode.EVERY_EXECUTION, result);
        ImmutableTaskGraph itg = tg.snapshot();
        TornadoExecutionPlan plan = new TornadoExecutionPlan(itg);
        plan.execute();
        System.out.println("SUM=" + result.get(0));
    }
}
