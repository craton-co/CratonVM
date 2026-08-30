import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Does a finished Thread become collectable, and does its finalizer run?
 *
 * `RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced` loops
 * `System.gc(); System.runFinalization(); Thread.sleep(50)` until a
 * `Thread.finalize()` override fires, and times out at 5 s. Four of its six
 * parameterisations time out on CratonVM and none on HotSpot. Three different
 * things produce that, and the test cannot tell them apart:
 *
 *   A. finalizers never run at all;
 *   B. finalizers run, but a dead Thread mirror is still strongly reachable;
 *   C. both work, and only netty's Recycler retains it.
 *
 * Each arm below isolates one. Arm 1 is a plain Object, arm 2 a bare Thread,
 * arm 3 a Thread that ran a body touching a ThreadLocal, arm 4 the same with a
 * live reference kept to something the thread allocated -- which is the shape
 * the netty test actually uses ("even if handled object is referenced").
 */
public final class ThreadCollectProbe {

    static final int TIMEOUT_MS = 5000;

    static final class Plain {
        final AtomicBoolean flag;
        Plain(AtomicBoolean f) { flag = f; }
        @Override protected void finalize() { flag.set(true); }
    }

    static final class Watched extends Thread {
        final AtomicBoolean flag;
        final Runnable body;
        Watched(AtomicBoolean f, Runnable b) { flag = f; body = b; }
        @Override public void run() { if (body != null) { body.run(); } }
        @Override protected void finalize() { flag.set(true); }
    }

    /** Spin the test's own collection loop; returns ms taken, or -1 on timeout. */
    static long await(AtomicBoolean flag) throws InterruptedException {
        long t0 = System.nanoTime();
        long deadline = t0 + TIMEOUT_MS * 1_000_000L;
        while (!flag.get()) {
            if (System.nanoTime() > deadline) {
                return -1;
            }
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
        }
        return (System.nanoTime() - t0) / 1_000_000L;
    }

    static void report(String arm, long ms) {
        System.out.printf("%-46s %s%n", arm, ms < 0 ? "TIMED OUT after " + TIMEOUT_MS + "ms" : "collected in " + ms + "ms");
    }

    static final ThreadLocal<Object> TL = new ThreadLocal<>();
    static final ThreadLocal<Object> INHERIT = new InheritableThreadLocal<>();
    static Object kept;

    public static void main(String[] args) throws Exception {
        // 1. a plain object's finalizer
        AtomicBoolean f1 = new AtomicBoolean();
        Plain p = new Plain(f1);
        p = null;
        report("1. plain Object.finalize", await(f1));

        // 2. a Thread that was never started
        AtomicBoolean f2 = new AtomicBoolean();
        Watched t2 = new Watched(f2, null);
        t2 = null;
        report("2. Thread never started", await(f2));

        // 3. a Thread that ran and finished
        AtomicBoolean f3 = new AtomicBoolean();
        Watched t3 = new Watched(f3, null);
        t3.start();
        t3.join();
        t3 = null;
        report("3. Thread started and joined", await(f3));

        // 4. …that touched a ThreadLocal
        AtomicBoolean f4 = new AtomicBoolean();
        Watched t4 = new Watched(f4, () -> TL.set(new Object()));
        t4.start();
        t4.join();
        t4 = null;
        report("4. Thread that set a ThreadLocal", await(f4));

        // 5. …and something it allocated is still referenced from a static
        AtomicBoolean f5 = new AtomicBoolean();
        Watched t5 = new Watched(f5, () -> { kept = new Object(); });
        t5.start();
        t5.join();
        t5 = null;
        report("5. Thread whose object is still held", await(f5));

        // 6. …that parked, i.e. went through LockSupport
        AtomicBoolean f6 = new AtomicBoolean();
        Watched t6 = new Watched(f6, () -> java.util.concurrent.locks.LockSupport.parkNanos(1_000_000L));
        t6.start();
        t6.join();
        t6 = null;
        report("6. Thread that parked", await(f6));

        // 7. …that synchronized on a monitor
        final Object lock = new Object();
        AtomicBoolean f7 = new AtomicBoolean();
        Watched t7 = new Watched(f7, () -> { synchronized (lock) { lock.notifyAll(); } });
        t7.start();
        t7.join();
        t7 = null;
        report("7. Thread that took a monitor", await(f7));

        System.out.println("(any TIMED OUT row is a mirror this VM still reaches, or a finalizer it never ran)");
        System.exit(0);
    }
}
