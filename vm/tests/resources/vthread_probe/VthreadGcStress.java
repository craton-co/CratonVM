import java.util.concurrent.CountDownLatch;
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
 * Usage: VthreadGcStress [threads=3000] [gcRounds=400] [gcSleepMillis=1]
 */
public final class VthreadGcStress {
    public static void main(String[] args) throws Exception {
        int threadCount = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        int gcRounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        long gcSleep = args.length > 2 ? Long.parseLong(args[2]) : 1L;
        CountDownLatch done = new CountDownLatch(threadCount);
        AtomicInteger counted = new AtomicInteger();
        Thread collector = new Thread(() -> {
            for (int i = 0; i < gcRounds; i++) {
                System.gc();
                try {
                    Thread.sleep(gcSleep);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }, "gc-driver");
        collector.setDaemon(true);
        collector.start();
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
        }
        done.await();
        boolean ok = counted.get() == threadCount;
        System.out.println("counted=" + counted.get() + " ok=" + ok);
        if (!ok) {
            throw new AssertionError("virtual thread loss");
        }
        System.out.println("OK");
    }
}
