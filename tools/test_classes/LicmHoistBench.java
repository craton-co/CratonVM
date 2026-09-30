// A/B microbench for the LICM aaload hoist: the inner loop reads m[j][i]
// with an invariant row m[j], so the row pointer is hoisted to the inner
// loop's preheader. The preheader (now including the null+bounds guard)
// executes once per inner() call; the loop body runs `cols` times per call.
// args: [cols] [outer]  — use cols=4096 (amortized) and cols=16 (worst case,
// guard every 16 iterations) to bound the guard's cost.
public class LicmHoistBench {
    static long inner(int[][] m, int j, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += m[j][i];
        }
        return s;
    }

    public static void main(String[] args) {
        final int rows = 8;
        final int cols = args.length > 0 ? Integer.parseInt(args[0]) : 4096;
        final int outer = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        int[][] m = new int[rows][cols];
        for (int r = 0; r < rows; r++)
            for (int c = 0; c < cols; c++) m[r][c] = r ^ c;

        long sink = 0;
        for (int w = 0; w < 20000; w++) sink += inner(m, w & 7, cols);

        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < 5; rep++) {
            long t0 = System.nanoTime();
            for (int o = 0; o < outer; o++) sink += inner(m, o & 7, cols);
            long dt = System.nanoTime() - t0;
            if (dt < best) best = dt;
        }
        System.out.println("cols=" + cols + " outer=" + outer
            + " best_ns=" + best + " sink=" + sink);
    }
}
