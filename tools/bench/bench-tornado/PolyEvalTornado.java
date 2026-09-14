// TornadoVM polynomial-eval kernel — 64 FMAs per element.
// Matches the HotSpot/CratonVM CpuPolyBench so the 4-way comparison
// runs the same arithmetic on each platform.
import uk.ac.manchester.tornado.api.ImmutableTaskGraph;
import uk.ac.manchester.tornado.api.TaskGraph;
import uk.ac.manchester.tornado.api.TornadoExecutionPlan;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;
import uk.ac.manchester.tornado.api.types.arrays.FloatArray;

public final class PolyEvalTornado {

    /** 64 fused multiply-adds per element. Same shape as CpuPolyBench. */
    public static void polyEval(FloatArray a, FloatArray b, FloatArray out) {
        for (@Parallel int i = 0; i < out.getSize(); i++) {
            float x = a.get(i);
            float c = b.get(i);
            float y = 1.0f;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            y = y * x + c;  y = y * x + c;  y = y * x + c;  y = y * x + c;
            out.set(i, y);
        }
    }

    public static void main(String[] args) {
        final int n     = args.length > 0 ? Integer.parseInt(args[0]) : 8388608;
        final int iters = args.length > 1 ? Integer.parseInt(args[1]) : 5;

        FloatArray a = new FloatArray(n);
        FloatArray b = new FloatArray(n);
        FloatArray c = new FloatArray(n);
        for (int i = 0; i < n; i++) {
            a.set(i, (float)((i % 100) * 0.01f));
            b.set(i, (float)((i % 50) * 0.02f));
        }

        TaskGraph tg = new TaskGraph("s0")
                .transferToDevice(DataTransferMode.FIRST_EXECUTION, a, b)
                .task("t0", PolyEvalTornado::polyEval, a, b, c)
                .transferToHost(DataTransferMode.EVERY_EXECUTION, c);

        ImmutableTaskGraph itg = tg.snapshot();

        try (TornadoExecutionPlan plan = new TornadoExecutionPlan(itg)) {
            plan.execute(); // warmup
            plan.execute();

            long totalNs = 0;
            long bestNs = Long.MAX_VALUE;
            for (int it = 0; it < iters; it++) {
                long t0 = System.nanoTime();
                plan.execute();
                long dt = System.nanoTime() - t0;
                totalNs += dt;
                if (dt < bestNs) bestNs = dt;
                System.out.printf("iter %2d: %8.3f ms%n", it, dt / 1_000_000.0);
            }
            long meanNs = totalNs / iters;
            System.out.println("best_ns=" + bestNs);
            System.out.println("mean_ns=" + meanNs);

            // CPU reference check on first + last element
            float xa = (float)((0 % 100) * 0.01f);
            float xb = (float)((0 % 50) * 0.02f);
            float yref0 = 1.0f;
            for (int k = 0; k < 64; k++) yref0 = yref0 * xa + xb;

            int last = n - 1;
            float xaN = (float)((last % 100) * 0.01f);
            float xbN = (float)((last % 50) * 0.02f);
            float yrefN = 1.0f;
            for (int k = 0; k < 64; k++) yrefN = yrefN * xaN + xbN;

            boolean ok = Math.abs(c.get(0) - yref0) < 1e-3f
                      && Math.abs(c.get(last) - yrefN) < 1e-3f;
            if (ok) {
                System.out.println("OK  c[0]=" + c.get(0) + "  c[" + last + "]=" + c.get(last));
                System.out.println("correctness=OK");
                System.exit(0);
            } else {
                System.out.println("FAIL c[0]=" + c.get(0) + " (ref " + yref0 + ")  c[" + last + "]=" + c.get(last) + " (ref " + yrefN + ")");
                System.out.println("correctness=FAIL");
                System.exit(1);
            }
        } catch (Exception e) {
            e.printStackTrace();
            System.exit(2);
        }
    }
}
