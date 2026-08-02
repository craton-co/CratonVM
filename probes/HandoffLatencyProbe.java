import java.util.Arrays;
import java.util.concurrent.Semaphore;
import java.util.concurrent.locks.LockSupport;

/**
 * Thread-to-thread wakeup latency, three ways.
 *
 * Written for TestAsyncMessagesPerformance, whose SEQ2 budget is the gap between
 * a 16 KB WebSocket message and the 4 KB message after it. The server endpoint
 * (TesterAsyncTiming) issues those back to back through a Semaphore:
 *
 *     semaphore.acquire(1);
 *     remote.sendBinary(LARGE_DATA, handler);   // handler releases on completion
 *     semaphore.acquire(1);
 *     remote.sendBinary(SMALL_DATA, handler);
 *
 * so SEQ2 is bounded below by one release -> acquire handoff. The test allows
 * 0.5 ms and CratonVM overruns it on ~80% of iterations, so measure the handoff
 * on its own: if it alone costs hundreds of microseconds, the WebSocket stack is
 * not where to look.
 *
 * Reports the median and the 99th percentile of a one-way wakeup, in
 * nanoseconds, over N ping-pong rounds. Each round is one park and one unpark
 * in each direction; the reported figure is per one-way handoff.
 *
 * usage: HandoffLatencyProbe [rounds]
 */
public class HandoffLatencyProbe {

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20000;

        // Warm the paths before measuring; the JIT must have compiled them.
        semaphore(2000);
        parkUnpark(2000);
        monitor(2000);

        report("Semaphore.release -> acquire", semaphore(rounds));
        report("LockSupport.unpark -> park  ", parkUnpark(rounds));
        report("Object.notify -> wait       ", monitor(rounds));
    }

    private static void report(String name, long[] samples) {
        Arrays.sort(samples);
        long med = samples[samples.length / 2];
        long p99 = samples[(int) (samples.length * 0.99)];
        long max = samples[samples.length - 1];
        System.out.printf("%s  n=%d  median=%,d ns  p99=%,d ns  max=%,d ns%n",
                name, samples.length, med, p99, max);
    }

    /** Round trip through a Semaphore, the shape TesterAsyncTiming uses. */
    private static long[] semaphore(int rounds) throws Exception {
        final Semaphore a = new Semaphore(0);
        final Semaphore b = new Semaphore(0);
        final long[] out = new long[rounds];
        Thread responder = new Thread(() -> {
            try {
                for (int i = 0; i < rounds; i++) {
                    a.acquire();
                    b.release();
                }
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }, "responder-sem");
        responder.setDaemon(true);
        responder.start();
        for (int i = 0; i < rounds; i++) {
            long t0 = System.nanoTime();
            a.release();
            b.acquire();
            out[i] = (System.nanoTime() - t0) / 2; // two one-way handoffs
        }
        responder.join(5000);
        return out;
    }

    private static long[] parkUnpark(int rounds) throws Exception {
        final long[] out = new long[rounds];
        final Thread[] holder = new Thread[1];
        final Thread main = Thread.currentThread();
        final boolean[] ping = new boolean[1];
        final boolean[] pong = new boolean[1];
        Thread responder = new Thread(() -> {
            for (int i = 0; i < rounds; i++) {
                while (!ping[0]) {
                    LockSupport.park();
                }
                ping[0] = false;
                pong[0] = true;
                LockSupport.unpark(main);
            }
        }, "responder-park");
        responder.setDaemon(true);
        holder[0] = responder;
        responder.start();
        for (int i = 0; i < rounds; i++) {
            long t0 = System.nanoTime();
            ping[0] = true;
            LockSupport.unpark(holder[0]);
            while (!pong[0]) {
                LockSupport.park();
            }
            pong[0] = false;
            out[i] = (System.nanoTime() - t0) / 2;
        }
        responder.join(5000);
        return out;
    }

    private static long[] monitor(int rounds) throws Exception {
        final Object lock = new Object();
        final int[] turn = { 0 };
        final long[] out = new long[rounds];
        Thread responder = new Thread(() -> {
            synchronized (lock) {
                try {
                    for (int i = 0; i < rounds; i++) {
                        while (turn[0] != 1) {
                            lock.wait();
                        }
                        turn[0] = 0;
                        lock.notifyAll();
                    }
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
        }, "responder-mon");
        responder.setDaemon(true);
        responder.start();
        synchronized (lock) {
            for (int i = 0; i < rounds; i++) {
                long t0 = System.nanoTime();
                turn[0] = 1;
                lock.notifyAll();
                while (turn[0] != 0) {
                    lock.wait();
                }
                out[i] = (System.nanoTime() - t0) / 2;
            }
        }
        responder.join(5000);
        return out;
    }
}
