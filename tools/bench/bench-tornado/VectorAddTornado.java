/*
 * VectorAddTornado.java
 *
 * Minimal TornadoVM 4.0.x hello-world: out[i] = a[i] + b[i] on the GPU
 * via the PTX backend on an NVIDIA device.
 *
 * Usage (after `source /c/craton/tornadovm/setvars.sh`):
 *   tornado --threadInfo -cp . VectorAddTornado [n] [iters]
 *
 * Defaults: n=1024, iters=10.
 * Exit code: 0 if all elements match the CPU reference, 1 otherwise.
 */

import uk.ac.manchester.tornado.api.ImmutableTaskGraph;
import uk.ac.manchester.tornado.api.TaskGraph;
import uk.ac.manchester.tornado.api.TornadoExecutionPlan;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;
import uk.ac.manchester.tornado.api.types.arrays.FloatArray;

public final class VectorAddTornado {

    /** Kernel: each i runs as a parallel GPU thread. */
    public static void vectorAdd(FloatArray a, FloatArray b, FloatArray c) {
        for (@Parallel int i = 0; i < c.getSize(); i++) {
            c.set(i, a.get(i) + b.get(i));
        }
    }

    public static void main(String[] args) {
        final int n     = args.length > 0 ? Integer.parseInt(args[0]) : 1024;
        final int iters = args.length > 1 ? Integer.parseInt(args[1]) : 10;

        FloatArray a = new FloatArray(n);
        FloatArray b = new FloatArray(n);
        FloatArray c = new FloatArray(n);
        for (int i = 0; i < n; i++) {
            a.set(i, (float) i);
            b.set(i, 2.0f * i + 1.0f);
        }

        TaskGraph tg = new TaskGraph("s0")
                .transferToDevice(DataTransferMode.FIRST_EXECUTION, a, b)
                .task("t0", VectorAddTornado::vectorAdd, a, b, c)
                .transferToHost(DataTransferMode.EVERY_EXECUTION, c);

        ImmutableTaskGraph itg = tg.snapshot();

        try (TornadoExecutionPlan plan = new TornadoExecutionPlan(itg)) {
            // Warm-up so JIT + PTX kernel + device alloc are not counted.
            plan.execute();

            long totalNs = 0;
            long bestNs = Long.MAX_VALUE;
            for (int it = 0; it < iters; it++) {
                long t0 = System.nanoTime();
                plan.execute();
                long dt = System.nanoTime() - t0;
                totalNs += dt;
                if (dt < bestNs) bestNs = dt;
                System.out.printf("iter %2d: %8.3f us%n", it, dt / 1_000.0);
            }
            long meanNs = totalNs / iters;
            System.out.printf("avg: %8.3f us  (n=%d, iters=%d)%n",
                    totalNs / 1_000.0 / iters, n, iters);
            System.out.println("best_ns=" + bestNs);
            System.out.println("mean_ns=" + meanNs);

            // Correctness check vs scalar CPU reference.
            int mismatches = 0;
            for (int i = 0; i < n; i++) {
                float expected = (float) i + (2.0f * i + 1.0f);
                if (c.get(i) != expected) {
                    if (mismatches < 8) {
                        System.out.printf("MISMATCH at %d: got=%f want=%f%n",
                                i, c.get(i), expected);
                    }
                    mismatches++;
                }
            }
            if (mismatches == 0) {
                System.out.println("OK  c[0]=" + c.get(0) + "  c[" + (n - 1) + "]=" + c.get(n - 1)
                        + "  (n=" + n + ")");
                System.out.println("correctness=OK");
                System.exit(0);
            } else {
                System.out.println("FAIL  " + mismatches + " mismatches");
                System.out.println("correctness=FAIL");
                System.exit(1);
            }
        } catch (Exception e) {
            e.printStackTrace();
            System.exit(2);
        }
    }
}
