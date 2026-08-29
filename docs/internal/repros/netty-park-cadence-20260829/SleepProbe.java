import java.util.concurrent.*;
import java.util.Arrays;

public class SleepProbe {
    public static void main(String[] a) throws Exception {
        // 1. Thread.sleep(50) precision
        long[] d = new long[40];
        for (int i = 0; i < 40; i++) {
            long t0 = System.nanoTime();
            Thread.sleep(50);
            d[i] = System.nanoTime() - t0;
        }
        report("Thread.sleep(50)", d, 50_000_000L);

        // 2. Thread.sleep(10)
        long[] d2 = new long[40];
        for (int i = 0; i < 40; i++) {
            long t0 = System.nanoTime();
            Thread.sleep(10);
            d2[i] = System.nanoTime() - t0;
        }
        report("Thread.sleep(10)", d2, 10_000_000L);

        // 3. ScheduledExecutorService.scheduleAtFixedRate period accuracy (50ms)
        final long[] prev = { 0 };
        final long[] gaps = new long[41];
        final int[] n = { 0 };
        final CountDownLatch latch = new CountDownLatch(41);
        ScheduledExecutorService ses = Executors.newSingleThreadScheduledExecutor();
        ses.scheduleAtFixedRate(() -> {
            long now = System.nanoTime();
            if (prev[0] != 0 && n[0] < gaps.length) { gaps[n[0]++] = now - prev[0]; }
            prev[0] = now;
            latch.countDown();
        }, 50, 50, TimeUnit.MILLISECONDS);
        latch.await(20, TimeUnit.SECONDS);
        ses.shutdownNow();
        report("scheduleAtFixedRate(50ms) gap", Arrays.copyOf(gaps, Math.max(1, n[0])), 50_000_000L);

        // 4. nanoTime resolution
        long minTick = Long.MAX_VALUE;
        for (int i = 0; i < 200000; i++) {
            long x = System.nanoTime(); long y;
            while ((y = System.nanoTime()) == x) { }
            long dt = y - x;
            if (dt > 0 && dt < minTick) minTick = dt;
        }
        System.out.println("nanoTime min observed tick = " + minTick + " ns");
    }

    static void report(String name, long[] d, long target) {
        long[] c = d.clone();
        Arrays.sort(c);
        long sum = 0; for (long v : c) sum += v;
        System.out.printf("%-30s n=%d  min=%.3fms  p50=%.3fms  mean=%.3fms  p90=%.3fms  max=%.3fms  meanOvershoot=%+.3fms%n",
            name, c.length, c[0]/1e6, c[c.length/2]/1e6, (sum/(double)c.length)/1e6,
            c[(int)(c.length*0.9)]/1e6, c[c.length-1]/1e6, ((sum/(double)c.length) - target)/1e6);
    }
}
