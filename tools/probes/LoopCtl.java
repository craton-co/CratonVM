public class LoopCtl {
    // Four independent accumulators: no serial recurrence, so the loop is
    // throughput-bound and the loop-control instructions are a real fraction.
    static int spin(int n) {
        int a = 0, b = 0, c = 0, d = 0;
        for (int i = 0; i < n; i++) { a ^= i; b += i; c |= i; d -= i; }
        return a + b + c + d;
    }
    public static void main(String[] x) {
        int reps = Integer.parseInt(x[0]);
        long t = 0;
        for (int w = 0; w < 50; w++) t += spin(20000);
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 7; r++) {
            long t0 = System.nanoTime();
            for (int k = 0; k < reps; k++) t += spin(20000);
            long dt = System.nanoTime() - t0;
            if (dt < best) best = dt;
        }
        System.out.println("ns_per_iter=" + (best / (double)(reps * 20000L)) + " checksum=" + t);
    }
}
