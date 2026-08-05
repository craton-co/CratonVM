import java.util.Arrays;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

/**
 * One-way wake latency, layer by layer, for the primitives on Tomcat's
 * WebSocket completion path.
 *
 * `ThreadPoolExecutor.execute() -> task entered` measured 568 us median on
 * CratonVM against 22 us on HotSpot, while raw LockSupport park/unpark was
 * only 12.8 us. Something between those two adds ~500 us. The layers, from
 * the bottom up:
 *
 *   LockSupport.unpark        -> park returns
 *   Object.notify             -> wait returns
 *   Condition.signal          -> await returns          (AQS ConditionObject)
 *   BlockingQueue.offer       -> take returns           (LBQ = lock + 2 conditions)
 *
 * The consumer stamps `received` the instant it wakes; the producer BUSY-SPINS
 * on that field, so the measurement never includes a second blocking handoff
 * on the way back.
 */
public class HandoffLayersProbe {

    private static final int WARMUP = 2000;
    private static final int ROUNDS = 20000;

    private static volatile long received;
    private static volatile boolean stop;

    private static void report(String label, long[] ns) {
        long[] s = ns.clone();
        Arrays.sort(s);
        System.out.printf("%-40s median %8.1f us   p90 %8.1f us   p99 %9.1f us   max %10.1f us%n",
                label, s[s.length / 2] / 1000.0, s[(int) (s.length * 0.90)] / 1000.0,
                s[(int) (s.length * 0.99)] / 1000.0, s[s.length - 1] / 1000.0);
    }

    /** Spin until the consumer publishes its receipt stamp. */
    private static long awaitReceipt() {
        long r;
        while ((r = received) == 0) {
            Thread.onSpinWait();
        }
        received = 0;
        return r;
    }

    private static void lockSupportPingPong() throws Exception {
        final Thread[] holder = new Thread[1];
        long[] d = new long[ROUNDS];
        Thread consumer = new Thread(() -> {
            while (!stop) {
                LockSupport.park();
                received = System.nanoTime();
            }
        });
        holder[0] = consumer;
        consumer.setDaemon(true);
        consumer.start();
        Thread.sleep(50);
        for (int i = 0; i < WARMUP + ROUNDS; i++) {
            long t0 = System.nanoTime();
            LockSupport.unpark(holder[0]);
            long r = awaitReceipt();
            if (i >= WARMUP) {
                d[i - WARMUP] = r - t0;
            }
        }
        stop = true;
        LockSupport.unpark(consumer);
        report("LockSupport.unpark -> park returns", d);
        stop = false;
    }

    private static void monitorPingPong() throws Exception {
        final Object lock = new Object();
        final boolean[] flag = new boolean[1];
        long[] d = new long[ROUNDS];
        Thread consumer = new Thread(() -> {
            synchronized (lock) {
                while (!stop) {
                    while (!flag[0] && !stop) {
                        try {
                            lock.wait();
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                    flag[0] = false;
                    received = System.nanoTime();
                }
            }
        });
        consumer.setDaemon(true);
        consumer.start();
        Thread.sleep(50);
        for (int i = 0; i < WARMUP + ROUNDS; i++) {
            long t0 = System.nanoTime();
            synchronized (lock) {
                flag[0] = true;
                lock.notify();
            }
            long r = awaitReceipt();
            if (i >= WARMUP) {
                d[i - WARMUP] = r - t0;
            }
        }
        stop = true;
        synchronized (lock) {
            lock.notifyAll();
        }
        report("Object.notify -> wait returns", d);
        stop = false;
    }

    private static void conditionPingPong() throws Exception {
        final ReentrantLock lock = new ReentrantLock();
        final Condition cond = lock.newCondition();
        final boolean[] flag = new boolean[1];
        long[] d = new long[ROUNDS];
        Thread consumer = new Thread(() -> {
            lock.lock();
            try {
                while (!stop) {
                    while (!flag[0] && !stop) {
                        cond.awaitUninterruptibly();
                    }
                    flag[0] = false;
                    received = System.nanoTime();
                }
            } finally {
                lock.unlock();
            }
        });
        consumer.setDaemon(true);
        consumer.start();
        Thread.sleep(50);
        for (int i = 0; i < WARMUP + ROUNDS; i++) {
            long t0 = System.nanoTime();
            lock.lock();
            try {
                flag[0] = true;
                cond.signal();
            } finally {
                lock.unlock();
            }
            long r = awaitReceipt();
            if (i >= WARMUP) {
                d[i - WARMUP] = r - t0;
            }
        }
        stop = true;
        lock.lock();
        try {
            cond.signalAll();
        } finally {
            lock.unlock();
        }
        report("Condition.signal -> await returns", d);
        stop = false;
    }

    private static void queuePingPong(String label, BlockingQueue<Object> q) throws Exception {
        final Object token = new Object();
        long[] d = new long[ROUNDS];
        Thread consumer = new Thread(() -> {
            try {
                while (!stop) {
                    q.take();
                    received = System.nanoTime();
                }
            } catch (InterruptedException e) {
                // done
            }
        });
        consumer.setDaemon(true);
        consumer.start();
        Thread.sleep(50);
        for (int i = 0; i < WARMUP + ROUNDS; i++) {
            long t0 = System.nanoTime();
            q.put(token);
            long r = awaitReceipt();
            if (i >= WARMUP) {
                d[i - WARMUP] = r - t0;
            }
        }
        stop = true;
        consumer.interrupt();
        report(label + ".put -> take returns", d);
        stop = false;
    }

    public static void main(String[] args) throws Exception {
        lockSupportPingPong();
        monitorPingPong();
        conditionPingPong();
        queuePingPong("LinkedBlockingQueue", new LinkedBlockingQueue<>());
        queuePingPong("ArrayBlockingQueue", new ArrayBlockingQueue<>(64));
        queuePingPong("SynchronousQueue", new SynchronousQueue<>());
    }
}
