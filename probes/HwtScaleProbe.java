import io.netty.util.HashedWheelTimer;
import io.netty.util.Timeout;
import io.netty.util.TimerTask;

import java.util.Arrays;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

/**
 * Scale decomposition of HashedWheelTimerTest#testExecutionOnTime.
 *
 * The test itself only ever runs N=100000 and only reports the first delay that
 * exceeds the bound.  This runs the identical shape at several N and prints the
 * whole distribution, so an off-by-one bucket error (max delay wrong at EVERY N)
 * can be told apart from an overload cascade (max delay grows with N).
 *
 * Theoretical max for a correct wheel: tickDuration + timeout = 325 ms.
 */
public final class HwtScaleProbe {

    public static void main(String[] args) throws Exception {
        int[] counts = { 100, 1000, 10000, 100000 };
        if (args.length > 0) {
            String[] parts = args[0].split(",");
            counts = new int[parts.length];
            for (int i = 0; i < parts.length; i++) {
                counts[i] = Integer.parseInt(parts[i].trim());
            }
        }
        int reps = args.length > 1 ? Integer.parseInt(args[1].trim()) : 1;
        for (int r = 0; r < reps; r++) {
            for (int n : counts) {
                run(n);
            }
        }
    }

    private static void run(int scheduledTasks) throws Exception {
        final int tickDuration = 200;
        final int timeout = 125;
        final int maxTimeout = 2 * (tickDuration + timeout);

        final HashedWheelTimer timer = new HashedWheelTimer(tickDuration, TimeUnit.MILLISECONDS);
        final BlockingQueue<Long> queue = new LinkedBlockingQueue<Long>();

        long schedStart = System.nanoTime();
        for (int i = 0; i < scheduledTasks; i++) {
            final long start = System.nanoTime();
            timer.newTimeout(new TimerTask() {
                @Override
                public void run(final Timeout t) throws Exception {
                    queue.add(TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - start));
                }
            }, timeout, TimeUnit.MILLISECONDS);
        }
        long schedMs = (System.nanoTime() - schedStart) / 1000000L;

        long[] delays = new long[scheduledTasks];
        long drainStart = System.nanoTime();
        for (int i = 0; i < scheduledTasks; i++) {
            delays[i] = queue.take();
        }
        long drainMs = (System.nanoTime() - drainStart) / 1000000L;

        long[] sorted = delays.clone();
        Arrays.sort(sorted);
        int over = 0;
        int underMin = 0;
        for (long d : sorted) {
            if (d >= maxTimeout) {
                over++;
            }
            if (d < timeout) {
                underMin++;
            }
        }
        System.out.println("n=" + scheduledTasks
                + " schedMs=" + schedMs
                + " drainMs=" + drainMs
                + " min=" + sorted[0]
                + " p50=" + sorted[scheduledTasks / 2]
                + " p90=" + sorted[(int) (scheduledTasks * 0.90)]
                + " p99=" + sorted[(int) (scheduledTasks * 0.99)]
                + " max=" + sorted[scheduledTasks - 1]
                + " over(>=" + maxTimeout + ")=" + over
                + " under(<" + timeout + ")=" + underMin
                + "  [theoretical max for a correct wheel = " + (tickDuration + timeout) + "]");
        timer.stop();
    }
}
