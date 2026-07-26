import java.util.WeakHashMap;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.CountDownLatch;

public class WhmRepro3 {
    static final Object lock = new Object();
    static final WeakHashMap<Thread, String> map = new WeakHashMap<>();
    static final AtomicInteger misses = new AtomicInteger(0);

    static String getSelf() {
        synchronized (lock) {
            return map.get(Thread.currentThread());
        }
    }

    static void putSelf(String v) {
        synchronized (lock) {
            map.put(Thread.currentThread(), v);
        }
    }

    public static void main(String[] args) throws Exception {
        putSelf("main-value");

        int numThreads = 8;
        int itersPerThread = 20000;
        CountDownLatch done = new CountDownLatch(numThreads);
        for (int t = 0; t < numThreads; t++) {
            final int id = t;
            Thread worker = new Thread(() -> {
                putSelf("worker-" + id + "-value");
                for (int i = 0; i < itersPerThread; i++) {
                    String v = getSelf();
                    if (v == null) {
                        int m = misses.incrementAndGet();
                        if (m <= 20) {
                            System.out.println("MISS worker=" + id + " iter=" + i
                                + " mapSize=" + map.size());
                        }
                    }
                    // Also churn the table so resize/rehash keeps happening
                    // concurrently with other threads' get() calls.
                    if ((i & 0xFF) == 0) {
                        synchronized (lock) {
                            map.put(new Thread("churn-" + id + "-" + i), "churn");
                        }
                    }
                }
                done.countDown();
            });
            worker.setDaemon(true);
            worker.start();
        }
        done.await();

        // Finally check main's own entry, like RandomizedContext's suite
        // thread checking its OWN entry after all workers finished.
        String mainVal = getSelf();
        System.out.println("main getSelf() after workers = " + mainVal);

        System.out.println("Done. misses=" + misses.get() + " mapSize=" + map.size());
        if (misses.get() > 0 || mainVal == null) {
            System.exit(1);
        }
    }
}
