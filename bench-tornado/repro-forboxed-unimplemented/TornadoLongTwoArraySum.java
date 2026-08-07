// Isolating repro (works): two-array LongArray + LongArray -> LongArray
// reduce, no widening cast, no multiply. See README.md in this directory.
package uk.ac.manchester.tornado.examples;
import uk.ac.manchester.tornado.api.*;
import uk.ac.manchester.tornado.api.types.arrays.LongArray;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.annotations.Reduce;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;

public class TornadoLongTwoArraySum {
    private static void sumReduce(LongArray a, LongArray b, @Reduce LongArray result) {
        for (@Parallel int i = 0; i < a.getSize(); i++) {
            result.set(0, result.get(0) + a.get(i) + b.get(i));
        }
    }

    public static void main(String[] args) throws Exception {
        int n = 1024;
        LongArray a = new LongArray(n);
        LongArray b = new LongArray(n);
        LongArray result = new LongArray(1);
        for (int i = 0; i < n; i++) { a.set(i, i); b.set(i, 1); }
        result.init(0);

        TaskGraph tg = new TaskGraph("s0")
            .transferToDevice(DataTransferMode.EVERY_EXECUTION, a, b)
            .task("t0", TornadoLongTwoArraySum::sumReduce, a, b, result)
            .transferToHost(DataTransferMode.EVERY_EXECUTION, result);
        TornadoExecutionPlan plan = new TornadoExecutionPlan(tg.snapshot());
        plan.execute();
        System.out.println("SUM=" + result.get(0));
    }
}
