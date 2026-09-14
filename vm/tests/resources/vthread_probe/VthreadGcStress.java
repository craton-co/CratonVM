import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Virtual threads yielding (Thread.sleep) while a platform thread drives
 * stop-the-world collections.
 *
 * The point is the WINDOW between a virtual thread's last per-bytecode
 * safepoint poll and the `deposit_root_snapshot()` that its carrier performs
 * when the continuation unmounts. A pause requested inside that window counts
 * the continuation's ThreadId in `expected` and then never sees it again: the
 * carrier goes back to waiting for work, `arrived` can never reach `expected`,
 * and the whole VM freezes with no output.
 *
 * `VthreadProbe` hits that window by luck (about one run in five). This probe
 * hits it on purpose by requesting pauses continuously, so the hang is a
 * near-certainty rather than a coin flip -- which is what makes it usable as
 * a regression gate.
 *
 * <p>It prints `progress remaining=` for the reason given at length in
 * `VthreadProbe.java`: the freeze it gates against makes NO progress from any
 * thread, and a debug build merely makes slow progress, so the harness watches
 * the counter rather than the clock.</p>
 *
 * Usage: VthreadGcStress [threads=3000] [gcRounds=400] [gcSleepMillis=1]
 */
public final class VthreadGcStress {
    private static void heartbeat(long unspawned, CountDownLatch done, AtomicInteger counted,
                                  AtomicInteger pauses, long t0) {
        System.out.println("progress remaining=" + (unspawned + done.getCount())
                + " unspawned=" + unspawned
                + " counted=" + counted.get()
                + " pauses=" + pauses.get()
                + " elapsedMs=" + ((System.nanoTime() - t0) / 1_000_000L));
    }

    public static void main(String[] args) throws Exception {
        int threadCount = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        int gcRounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        long gcSleep = args.length > 2 ? Long.parseLong(args[2]) : 1L;
        CountDownLatch done = new CountDownLatch(threadCount);
        AtomicInteger counted = new AtomicInteger();
        AtomicInteger pauses = new AtomicInteger();
        long t0 = System.nanoTime();
        Thread collector = new Thread(() -> {
            for (int i = 0; i < gcRounds; i++) {
                System.gc();
                pauses.incrementAndGet();
                try {
                    Thread.sleep(gcSleep);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }, "gc-driver");
        collector.setDaemon(true);
        collector.start();
        int every = Math.max(1, threadCount / 200);
        for (int i = 0; i < threadCount; i++) {
            Thread.startVirtualThread(() -> {
                try {
                    Thread.sleep(5);
                    counted.incrementAndGet();
                } catch (InterruptedException e) {
                    throw new AssertionError(e);
                } finally {
                    done.countDown();
                }
            });
            if ((i + 1) % every == 0) {
                heartbeat(threadCount - (i + 1), done, counted, pauses, t0);
            }
        }
        while (!done.await(1, TimeUnit.SECONDS)) {
            heartbeat(0, done, counted, pauses, t0);
        }
        heartbeat(0, done, counted, pauses, t0);
        boolean ok = counted.get() == threadCount;
        System.out.println("counted=" + counted.get() + " ok=" + ok);
        if (!ok) {
            throw new AssertionError("virtual thread loss");
        }
        System.out.println("OK");
    }
}
