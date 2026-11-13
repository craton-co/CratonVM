// j27 wave 3 (lane BACKLOG, TV2-5): the VM's thread-task reader
// (`read_thread_task_object`, used to recognise a ThreadPoolExecutor worker
// at thread start) now reads Thread.holder / Thread.target through the
// per-VM thread-layout memo. What a started thread runs must not change: a
// Runnable handed to Thread, a subclass declaring its OWN `target` /
// `holder` fields (look-alikes that must not be taken for Thread's), and
// pool workers.
// Page: docs/feature-designs/jit-ir-optimizations.md (TV2-5)
//
// Expected HotSpot 25 output (facts only):
//   fact plain-runnable ran
//   fact lookalike-sub ran-super-target
//   fact lookalike-fields own-target,own-holder
//   fact override-run ran-override
//   fact pool-sum 4950
//   fact pool-worker-names true
public class J27ThreadTaskRows {
    static final StringBuilder LOG = new StringBuilder();

    static final class LookAlike extends Thread {
        // Same names as java.lang.Thread's private fields.
        final Object target = "own-target";
        final Object holder = "own-holder";

        LookAlike(Runnable r) {
            super(r);
        }
    }

    static final class OwnRun extends Thread {
        public void run() {
            synchronized (LOG) {
                LOG.setLength(0);
                LOG.append("ran-override");
            }
        }
    }

    static String runAndRead(Thread t) throws InterruptedException {
        t.start();
        t.join();
        synchronized (LOG) {
            return LOG.toString();
        }
    }

    static Runnable logging(String what) {
        return () -> {
            synchronized (LOG) {
                LOG.setLength(0);
                LOG.append(what);
            }
        };
    }

    public static void main(String[] args) throws Exception {
        System.out.println("fact plain-runnable " + runAndRead(new Thread(logging("ran"))));
        LookAlike la = new LookAlike(logging("ran-super-target"));
        System.out.println("fact lookalike-sub " + runAndRead(la));
        System.out.println("fact lookalike-fields " + la.target + "," + la.holder);
        System.out.println("fact override-run " + runAndRead(new OwnRun()));

        java.util.concurrent.ExecutorService pool =
                java.util.concurrent.Executors.newFixedThreadPool(3);
        java.util.List<java.util.concurrent.Future<String>> names = new java.util.ArrayList<>();
        java.util.concurrent.atomic.AtomicInteger sum = new java.util.concurrent.atomic.AtomicInteger();
        for (int i = 0; i < 100; i++) {
            final int v = i;
            names.add(pool.submit(() -> {
                sum.addAndGet(v);
                return Thread.currentThread().getName();
            }));
        }
        boolean allPool = true;
        for (java.util.concurrent.Future<String> f : names) {
            allPool &= f.get().startsWith("pool-");
        }
        pool.shutdown();
        pool.awaitTermination(30, java.util.concurrent.TimeUnit.SECONDS);
        System.out.println("fact pool-sum " + sum.get());
        System.out.println("fact pool-worker-names " + allPool);
    }
}
