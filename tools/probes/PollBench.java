public class PollBench {
    static long hotLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += i ^ (s >>> 3);
        return s;
    }
    public static void main(String[] a) {
        int reps = Integer.parseInt(a[0]);
        long t = 0;
        for (int w = 0; w < 50; w++) t += hotLoop(20000);       // warm
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 7; r++) {
            long t0 = System.nanoTime();
            for (int k = 0; k < reps; k++) t += hotLoop(20000);
            long d = System.nanoTime() - t0;
            if (d < best) best = d;
        }
        System.out.println("ns_per_iter=" + (best / (double)(reps * 20000L)) + " checksum=" + t);
    }
}
