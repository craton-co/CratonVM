// Isolating repro (FAILS): single-array IntArray -> LongArray reduce with
// one (long) widening cast. Throws:
//   uk.ac.manchester.tornado.api.exceptions.TornadoInternalError: unimplemented
//     at TornadoSnippetReflectionProvider.forBoxed
//     at PTXGPUReduceSnippets$Templates.lower
// This is the minimal reproduction of the gap documented in README.md in
// this directory -- the smallest kernel that still triggers it. Suitable
// as-is for an upstream bug report against beehive-lab/TornadoVM.
package uk.ac.manchester.tornado.examples;
import uk.ac.manchester.tornado.api.*;
import uk.ac.manchester.tornado.api.types.arrays.IntArray;
import uk.ac.manchester.tornado.api.types.arrays.LongArray;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.annotations.Reduce;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;

public class TornadoIntToLongCastSum {
    private static void sumReduce(IntArray a, @Reduce LongArray result) {
        for (@Parallel int i = 0; i < a.getSize(); i++) {
            result.set(0, result.get(0) + (long) a.get(i));
        }
    }

    public static void main(String[] args) throws Exception {
        int n = 1024;
        IntArray a = new IntArray(n);
        LongArray result = new LongArray(1);
        for (int i = 0; i < n; i++) a.set(i, i);
        result.init(0);

        TaskGraph tg = new TaskGraph("s0")
            .transferToDevice(DataTransferMode.EVERY_EXECUTION, a)
            .task("t0", TornadoIntToLongCastSum::sumReduce, a, result)
            .transferToHost(DataTransferMode.EVERY_EXECUTION, result);
        TornadoExecutionPlan plan = new TornadoExecutionPlan(tg.snapshot());
        plan.execute();
        System.out.println("SUM=" + result.get(0));
    }
}
