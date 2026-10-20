import java.net.URL;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * gci i18/vp: do N virtual threads that park INSIDE a JDK (or application) body that
 * CratonVM runs nested under one of its own natives all get to run their first
 * statement? (page gci-i15orch-virtual-threads-never-start-under-nojit-so-300-thread-probes-hang-FIXED-20261015,
 * i17-vt report section 7 item 2).
 *
 * Every worker counts STARTED down as its first statement, then calls a JDK method whose
 * body reaches application code that parks until main opens the GO gate, then counts DONE
 * down. On HotSpot 25 none of these calls has a native frame, so every park unmounts and
 * all N workers start whatever N is. A VM whose native stands in front of the body and
 * counts as an opaque native frame pins each park to its carrier instead, so at most
 * "carriers" workers reach the gate and the rest never start. Main waits at most WAIT
 * seconds for the starts (it never hangs), opens the gate, waits again, and prints
 *
 *   vp18 MODE threads=N started=S done=D
 *
 * (stdout is a pure function of the args on HotSpot: S = D = N). Diagnostics on stderr.
 *
 * Modes:
 *   tl        ThreadLocal.get() of a withInitial ThreadLocal whose supplier parks
 *             (the --jdk-only real-field ThreadLocal.get Intrinsic declines a miss into
 *             the JDK's get(Thread), which runs initialValue()).
 *   resource  ClassLoader.getResource(name) on a user loader (parent null) whose
 *             findResource parks (the getResource native runs the JDK body for a user
 *             loader under --jdk-only).
 *   loadclass ClassLoader.loadClass(name) on a user loader whose loadClass(String, boolean)
 *             override parks, then throws ClassNotFoundException (the JDK's
 *             loadClass(String) is `return loadClass(name, false)`).
 *   await     ThreadPoolExecutor.awaitTermination on a one-thread pool whose task waits
 *             for the gate; main shuts the pool down after opening it.
 *   mixed     id % 3: tl, resource, loadclass.
 *
 * Usage: GciVp18NestedBodyPinProbe MODE [N [WAIT]]   (N default 300, WAIT default 60)
 */
public class GciVp18NestedBodyPinProbe {
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static final CountDownLatch GO = new CountDownLatch(1);
    static CountDownLatch STARTED;
    static CountDownLatch DONE;
    static int waitSeconds = 60;
    static ThreadPoolExecutor POOL;
    static final ThreadLocal<Integer> TL = ThreadLocal.withInitial(() -> {
        gate();
        return Integer.valueOf(18);
    });
    static final GateLoader LOADER = new GateLoader();

    /** Park until main opens the gate (bounded: a lost wake-up is a mismatch, not a hang). */
    static void gate() {
        try {
            if (!GO.await(2L * waitSeconds, TimeUnit.SECONDS)) {
                MISMATCH.incrementAndGet();
            }
        } catch (InterruptedException e) {
            MISMATCH.incrementAndGet();
        }
    }

    static final class GateLoader extends ClassLoader {
        GateLoader() {
            super("gci-vp18", null);
        }

        @Override
        protected URL findResource(String name) {
            gate();
            return null;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.startsWith("gci.vp18.")) {
                gate();
                throw new ClassNotFoundException(name);
            }
            return super.loadClass(name, resolve);
        }
    }

    static void viaThreadLocal() {
        Integer v = TL.get();
        if (v == null || v.intValue() != 18) {
            MISMATCH.incrementAndGet();
        }
    }

    static void viaResource(int id) {
        URL u = LOADER.getResource("gci/vp18/r" + id);
        if (u != null) {
            MISMATCH.incrementAndGet();
        }
    }

    static void viaLoadClass(int id) {
        try {
            LOADER.loadClass("gci.vp18.Gate" + id);
            MISMATCH.incrementAndGet();
        } catch (ClassNotFoundException expected) {
            // the override's answer
        }
    }

    static void viaAwait() {
        try {
            if (!POOL.awaitTermination(2L * waitSeconds, TimeUnit.SECONDS)) {
                MISMATCH.incrementAndGet();
            }
        } catch (InterruptedException e) {
            MISMATCH.incrementAndGet();
        }
    }

    static int kindOf(String mode, int id) {
        switch (mode) {
            case "tl":
                return 0;
            case "resource":
                return 1;
            case "loadclass":
                return 2;
            case "await":
                return 3;
            case "mixed":
                return id % 3;
            default:
                throw new IllegalArgumentException("mode: tl|resource|loadclass|await|mixed");
        }
    }

    static void worker(int kind, int id) {
        STARTED.countDown();
        switch (kind) {
            case 0:
                viaThreadLocal();
                break;
            case 1:
                viaResource(id);
                break;
            case 2:
                viaLoadClass(id);
                break;
            default:
                viaAwait();
                break;
        }
        DONE.countDown();
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "tl";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 300;
        waitSeconds = args.length > 2 ? Integer.parseInt(args[2]) : 60;
        kindOf(mode, 0);
        STARTED = new CountDownLatch(n);
        DONE = new CountDownLatch(n);
        POOL = new ThreadPoolExecutor(1, 1, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<>());
        POOL.execute(GciVp18NestedBodyPinProbe::gate);
        ThreadFactory factory = Thread.ofVirtual().factory();
        Thread[] threads = new Thread[n];
        long t0 = System.nanoTime();
        for (int id = 0; id < n; id++) {
            final int fid = id;
            final int kind = kindOf(mode, id);
            threads[id] = factory.newThread(() -> worker(kind, fid));
            threads[id].start();
        }
        STARTED.await(waitSeconds, TimeUnit.SECONDS);
        long started = n - STARTED.getCount();
        long t1 = System.nanoTime();
        // Open the gate for everyone, started or not, then let the pool finish.
        GO.countDown();
        POOL.shutdown();
        DONE.await(waitSeconds, TimeUnit.SECONDS);
        long done = n - DONE.getCount();
        System.out.println("vp18 " + mode + " threads=" + n + " started=" + started + " done=" + done);
        System.err.println("[vp18] " + mode + " mismatches=" + MISMATCH.get()
                + " start_ms=" + (t1 - t0) / 1_000_000
                + " carriers=" + Runtime.getRuntime().availableProcessors());
        System.exit(0);
    }
}
