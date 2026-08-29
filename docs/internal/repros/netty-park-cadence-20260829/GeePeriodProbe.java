package io.netty.util.concurrent;

import java.util.Arrays;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/** Measures the actual firing period of GlobalEventExecutor.scheduleAtFixedRate(50ms). */
public final class GeePeriodProbe {
    public static void main(String[] a) throws Exception {
        final int N = 80;
        final long[] fire = new long[N];
        final int[] n = { 0 };
        final CountDownLatch latch = new CountDownLatch(N);
        ScheduledFuture<?> f = GlobalEventExecutor.INSTANCE.scheduleAtFixedRate(new Runnable() {
            @Override public void run() {
                if (n[0] < N) { fire[n[0]++] = System.nanoTime(); }
                latch.countDown();
            }
        }, 50, 50, TimeUnit.MILLISECONDS);
        latch.await(30, TimeUnit.SECONDS);
        f.cancel(false);
        int c = n[0];
        long[] gaps = new long[c - 1];
        for (int i = 1; i < c; i++) { gaps[i - 1] = fire[i] - fire[i - 1]; }
        long[] s = gaps.clone();
        Arrays.sort(s);
        long sum = 0; for (long v : s) { sum += v; }
        System.out.printf("GlobalEventExecutor fixedRate(50ms): n=%d min=%.3f p10=%.3f p50=%.3f mean=%.3f p90=%.3f max=%.3f  totalSpan=%.3fms over %d cycles => avg %.3fms%n",
            s.length, s[0]/1e6, s[s.length/10]/1e6, s[s.length/2]/1e6, (sum/(double)s.length)/1e6,
            s[(int)(s.length*0.9)]/1e6, s[s.length-1]/1e6,
            (fire[c-1]-fire[0])/1e6, c-1, ((fire[c-1]-fire[0])/(double)(c-1))/1e6);
        int under = 0; for (long g : gaps) { if (g < 49_000_000L) { under++; } }
        System.out.println("gaps under 49ms: " + under + "/" + gaps.length);
        System.out.println("first 20 gaps ms: " + Arrays.toString(Arrays.stream(gaps).limit(20).map(x -> x/100000).toArray()).replace("[","").replace("]",""));
        System.exit(0);
    }
}
