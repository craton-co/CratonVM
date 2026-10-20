// A private `ForkJoinPool` with a worker factory, the shape of the JDK 25
// virtual-thread scheduler (`VirtualThread.createDefaultScheduler`): asyncMode,
// `execute` / `externalSubmit`-style submissions from outside, a delayed
// `schedule`, the workers listed as platform threads while they live.
//
//   javac -d /tmp/vtc tools/probes/interp/L6/VtcForkJoinPool.java
//   java     -cp /tmp/vtc VtcForkJoinPool
//   cratonvm -cp /tmp/vtc VtcForkJoinPool
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.ForkJoinWorkerThread;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

public class VtcForkJoinPool {
    public static void main(String[] args) throws Exception {
        AtomicInteger made = new AtomicInteger();
        ForkJoinPool.ForkJoinWorkerThreadFactory factory = pool -> {
            made.incrementAndGet();
            return new ForkJoinWorkerThread(null, pool, true) { };
        };
        ForkJoinPool pool = new ForkJoinPool(2, factory, (t, e) -> { }, true,
                0, 256, 1, p -> true, 30, TimeUnit.SECONDS);
        AtomicInteger ran = new AtomicInteger();
        CountDownLatch all = new CountDownLatch(1000);
        for (int i = 0; i < 1000; i++) {
            pool.execute(() -> {
                ran.incrementAndGet();
                all.countDown();
            });
        }
        System.out.println("executed: " + all.await(60, TimeUnit.SECONDS) + " " + ran.get());
        String[] who = new String[1];
        CountDownLatch one = new CountDownLatch(1);
        pool.execute(() -> {
            who[0] = Thread.currentThread().getName().replaceAll("\\d+", "N") + " "
                    + (Thread.currentThread() instanceof ForkJoinWorkerThread) + " daemon="
                    + Thread.currentThread().isDaemon();
            one.countDown();
        });
        one.await(30, TimeUnit.SECONDS);
        System.out.println("worker: " + who[0]);
        boolean listed = false;
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if (t instanceof ForkJoinWorkerThread w && w.getPool() == pool) {
                listed = true;
            }
        }
        System.out.println("listed: " + listed);
        long t0 = System.nanoTime();
        ScheduledFuture<?> f = pool.schedule(() -> { }, 100, TimeUnit.MILLISECONDS);
        f.get(30, TimeUnit.SECONDS);
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("scheduled: " + (ms >= 90 && ms < 20_000));
        System.out.println("factory used: " + (made.get() >= 1));
        pool.shutdown();
        System.out.println("terminated: " + pool.awaitTermination(30, TimeUnit.SECONDS));
    }
}
