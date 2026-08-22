/**
 * Is Thread.sleep(ms) accurate on this VM, and what does System.nanoTime() cost?
 *
 * HashedWheelTimer's worker paces every tick with a single Thread.sleep of the
 * remaining milliseconds, so a systematic oversleep is a per-tick lag that no
 * amount of task-drain throughput can recover.
 */
public final class SleepAccuracyProbe {

    public static void main(String[] args) throws Exception {
        int[] naps = { 1, 2, 5, 10, 25, 50, 100, 200 };
        for (int rep = 0; rep < 3; rep++) {
            StringBuilder sb = new StringBuilder("sleep-overshoot-us rep=" + rep);
            for (int ms : naps) {
                long t0 = System.nanoTime();
                Thread.sleep(ms);
                long elapsedUs = (System.nanoTime() - t0) / 1000L;
                sb.append("  ").append(ms).append("ms->+").append(elapsedUs - ms * 1000L);
            }
            System.out.println(sb);
        }

        // nanoTime cost
        int iters = 2000000;
        long t0 = System.nanoTime();
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += System.nanoTime();
        }
        long ns = System.nanoTime() - t0;
        System.out.println("nanoTime ns/call=" + (ns / (double) iters) + " (sink " + (acc == 0 ? 1 : 0) + ")");

        // currentTimeMillis cost
        t0 = System.nanoTime();
        acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += System.currentTimeMillis();
        }
        ns = System.nanoTime() - t0;
        System.out.println("currentTimeMillis ns/call=" + (ns / (double) iters) + " (sink " + (acc == 0 ? 1 : 0) + ")");
    }
}
