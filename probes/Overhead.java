/**
 * What cross-thread `Thread.getStackTrace()` costs the thread being sampled.
 *
 * Interleaved and warmed: the same loop runs sampler-off then sampler-on inside
 * one process, three times, so the ratio is not a cross-process comparison and
 * not a warmup artefact. Sampling at 2 ms.
 *
 * MEASURED 2026-08-18 (`-Xmx1g`, ZGC): HotSpot 0.99-1.03, CratonVM after the
 * safepoint-publish fix 1.02-1.05. The unfixed VM measured 1.00-1.01 -- free,
 * because it did no work and returned nothing. See
 * fixed-bugs/getstacktrace-of-a-running-thread-returned-an-empty-array-FIXED-20260818.md
 *
 * Read `samples=` beside the ratio: a ratio of 1.00 with `samples=0` is not a
 * cheap profiler, it is a broken one.
 */
public class Overhead {
    static volatile long sink = 0;
    static void busy(long n) { for (long i = 0; i < n; i++) sink += i * 7; }
    public static void main(String[] a) throws Exception {
        long N = Long.parseLong(a.length > 0 ? a[0] : "20000000");
        for (int w = 0; w < 3; w++) busy(N);            // warm
        final Thread self = Thread.currentThread();
        for (int r = 0; r < 3; r++) {
            long t0 = System.nanoTime();
            busy(N);
            long off = (System.nanoTime() - t0) / 1000000;

            final boolean[] stop = {false};
            final int[] n = {0};
            Thread s = new Thread(() -> {
                while (!stop[0]) {
                    if (self.getStackTrace().length > 0) n[0]++;
                    try { Thread.sleep(2); } catch (InterruptedException e) { return; }
                }
            });
            s.setDaemon(true); s.start();
            t0 = System.nanoTime();
            busy(N);
            long on = (System.nanoTime() - t0) / 1000000;
            stop[0] = true; s.join(500);
            System.out.println("OVERHEAD round=" + r + " sampler_off=" + off + "ms sampler_on=" + on
                    + "ms ratio=" + String.format("%.2f", (double) on / Math.max(1, off))
                    + " samples=" + n[0]);
        }
        System.out.println("OVERHEAD sink=" + sink);
    }
}
