import java.util.*;

/**
 * The cross-thread stack paths `StackProbe` does not cover: a PARKED target,
 * `Thread.getAllStackTraces()` (the `dumpThreads` native), and a first look at
 * the cost of sampling a running thread.
 *
 * Companion to `StackProbe.java`; see
 * fixed-bugs/getstacktrace-of-a-running-thread-returned-an-empty-array-FIXED-20260818.md
 *
 * The parked-target line is a control: it worked BEFORE the fix and must keep
 * working, because the reader deliberately skips its safepoint when the target
 * is already blocked. (Its depth is short compared with HotSpot's — a separate,
 * pre-existing gap in the blocking-deposit capture, noted in the page above.)
 */
public class StackProbe2 {
    static volatile long sink = 0;
    static final Object LOCK = new Object();

    static void busy(long n) { for (long i = 0; i < n; i++) sink += i * 7; }

    public static void main(String[] a) throws Exception {
        long N = Long.parseLong(a.length > 0 ? a[0] : "20000000");

        // 1. A PARKED target must still report its blocking call site.
        Thread parked = new Thread(() -> {
            synchronized (LOCK) {
                try { LOCK.wait(20000); } catch (InterruptedException e) {}
            }
        }, "parked-target");
        parked.setDaemon(true);
        parked.start();
        Thread.sleep(500);
        StackTraceElement[] pst = parked.getStackTrace();
        System.out.println("SP2 parked frames=" + pst.length
                + " top=" + (pst.length > 0 ? pst[0].getClassName()+"."+pst[0].getMethodName() : "<empty>"));

        // 2. A RUNNING target, via getAllStackTraces (the dumpThreads path).
        final Thread runner = new Thread(() -> busy(Long.MAX_VALUE), "running-target");
        runner.setDaemon(true);
        runner.start();
        Thread.sleep(300);
        Map<Thread, StackTraceElement[]> all = Thread.getAllStackTraces();
        StackTraceElement[] rst = all.get(runner);
        System.out.println("SP2 getAllStackTraces running-target frames="
                + (rst == null ? "ABSENT" : rst.length)
                + " top=" + (rst != null && rst.length > 0
                        ? rst[0].getClassName()+"."+rst[0].getMethodName() : "<empty>"));
        System.out.println("SP2 getAllStackTraces threads=" + all.size());

        // 3. Overhead, roughly: same busy loop with and without a sampler.
        //    `Overhead.java` is the interleaved, warmed version — prefer it.
        long t0 = System.nanoTime();
        busy(N);
        long solo = (System.nanoTime() - t0) / 1000000;

        final boolean[] stop = {false};
        final int[] samples = {0};
        final Thread self = Thread.currentThread();
        Thread sampler = new Thread(() -> {
            while (!stop[0]) {
                if (self.getStackTrace().length > 0) samples[0]++;
                try { Thread.sleep(2); } catch (InterruptedException e) { return; }
            }
        });
        sampler.setDaemon(true);
        sampler.start();
        t0 = System.nanoTime();
        busy(N);
        long sampled = (System.nanoTime() - t0) / 1000000;
        stop[0] = true;
        System.out.println("SP2 overhead: solo=" + solo + "ms sampled=" + sampled
                + "ms (" + samples[0] + " non-empty samples)");
        System.out.println("SP2 sink=" + sink);
    }
}
