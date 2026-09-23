import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * 10000 virtual threads, each sleeping 10 ms and then incrementing a shared
 * counter.
 *
 * <h2>Why it prints progress</h2>
 *
 * The harness that runs this used to guard it with a wall-clock cap
 * ("fail if it has not exited in 60 s"), and that constant was measured
 * against a RELEASE binary while `cargo test --workspace` builds debug. On one
 * 8-core host on 2026-09-11 the same source took 6.3 s in release and 30-125 s
 * in debug, and a spell of load 40 turned a 52 s debug run into a 125 s one.
 * A cap cannot separate "slow" from "stuck", and raising it is exactly the
 * change that hides the 2026-09-05 hang this probe exists to catch.
 *
 * <p>So the probe reports its own progress: `remaining=` counts virtual
 * threads not yet spawned plus virtual threads not yet finished, which only
 * ever goes down, and the harness fails when it STOPS going down. Heartbeats
 * are emitted during the spawn loop too — spawning 10000 virtual threads took
 * 75-244 s of that debug run on its own, so a guard that only started
 * watching after the loop would be watching nothing for most of the run.</p>
 *
 * <p>One heartbeat per 1/200th of the thread count, not per 1/40th: at 1/40th
 * the largest gap between two consecutive advances across seven loaded-host
 * runs was 29.2 s, and the guard's budget has to be a multiple of that. The
 * lines are cheap (200 of them, ~60 bytes each) and the harness reads them on
 * their own thread.</p>
 */
public final class VthreadProbe {
    private static void heartbeat(long unspawned, CountDownLatch done, AtomicInteger counted, long t0) {
        System.out.println("progress remaining=" + (unspawned + done.getCount())
                + " unspawned=" + unspawned
                + " counted=" + counted.get()
                + " elapsedMs=" + ((System.nanoTime() - t0) / 1_000_000L));
    }

    public static void main(String[] args) throws Exception {
        int threadCount = args.length == 0 ? 10_000 : Integer.parseInt(args[0]);
        CountDownLatch done = new CountDownLatch(threadCount);
        AtomicInteger counted = new AtomicInteger();
        long t0 = System.nanoTime();
        int every = Math.max(1, threadCount / 200);
        for (int i = 0; i < threadCount; i++) {
            Thread.startVirtualThread(() -> {
                try {
                    Thread.sleep(10);
                    counted.incrementAndGet();
                } catch (InterruptedException e) {
                    throw new AssertionError(e);
                } finally {
                    done.countDown();
                }
            });
            if ((i + 1) % every == 0) {
                heartbeat(threadCount - (i + 1), done, counted, t0);
            }
        }
        while (!done.await(1, TimeUnit.SECONDS)) {
            heartbeat(0, done, counted, t0);
        }
        heartbeat(0, done, counted, t0);
        boolean ok = counted.get() == threadCount;
        System.out.println("counted=" + counted.get() + " ok=" + ok);
        if (!ok) {
            throw new AssertionError("virtual thread loss");
        }
        System.out.println("OK");
    }
}
