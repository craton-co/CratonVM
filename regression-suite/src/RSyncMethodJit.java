import java.util.concurrent.CountDownLatch;

/**
 * Regression: an `ACC_SYNCHRONIZED` method must keep excluding after the JIT
 * compiles it.
 *
 * CratonVM's compiled bodies carry no monitor prologue/epilogue; the *caller*
 * supplies it (`JitSynchronizedMonitorGuard`, acquired in `execute_jit_call` /
 * `execute_jit_call_decoded`). For that reason the invocation-counter compile
 * path used to refuse `ACC_SYNCHRONIZED` methods outright — which is why every
 * layer Tomcat's BCEL annotation scan drives per byte
 * (`ByteArrayInputStream.read()` and friends, all synchronized one-liners) ran
 * interpreted, ~226x slower than HotSpot. See
 * docs/known-issues/tomcat/webapp-deploy-annotation-scan-interpreted-226x.md.
 *
 * Admitting them makes the guard load-bearing on a hot path, and a dropped or
 * double-released monitor is a SILENT wrong-answer bug, not a crash. This
 * vector is the check: the methods below are hammered hard enough to compile,
 * and every property that the implicit monitor is responsible for is asserted
 * against an exact expected value.
 *
 *   1. mutual exclusion, instance method   — no lost updates
 *   2. mutual exclusion, static method     — same, on the class mirror
 *   3. re-entrancy                         — a synchronized method calling
 *                                            another on the same receiver
 *   4. release on the exception path       — the guard's Drop must run, or the
 *                                            next acquirer deadlocks
 *   5. the monitor is really the receiver  — an outside `synchronized (obj)`
 *                                            block must exclude against it
 *
 * NB the counters are deliberately NOT atomic: a plain `++` under a correct
 * monitor is exact, and is the only thing that can detect a monitor that
 * stopped excluding.
 *
 * ⚠️ In the default CORE run the flag is OFF, so this exercises the ORDINARY
 * interpreted/synchronized path — a real check, but NOT the compiled one it was
 * written for. To cover that path it must be run explicitly:
 *
 *     CRATONVM_JIT=sync-methods ONLY=RSyncMethodJit bash regression-suite/run.sh
 *
 * Fold it into the default set only when that flag's default flips.
 */
public class RSyncMethodJit {

    private static final int THREADS = 4;
    private static final int ITERS = 60_000;

    private int instanceCount;
    private int reentrantCount;
    private static int staticCount;

    synchronized void bump() {
        instanceCount++;
    }

    synchronized void bumpTwice() {
        // Re-entrant: same receiver, already-held monitor.
        bump();
        reentrantCount++;
    }

    static synchronized void bumpStatic() {
        staticCount++;
    }

    synchronized int readInstance() {
        return instanceCount;
    }

    /** Always throws; the implicit monitor must still be released. */
    synchronized void thrower() {
        throw new IllegalStateException("expected");
    }

    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    public static void main(String[] args) throws Exception {
        RSyncMethodJit target = new RSyncMethodJit();

        // --- 1/2/3: exclusion + re-entrancy under real contention ----------
        CountDownLatch start = new CountDownLatch(1);
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            ts[t] = new Thread(() -> {
                try {
                    start.await();
                } catch (InterruptedException ex) {
                    Thread.currentThread().interrupt();
                    return;
                }
                for (int i = 0; i < ITERS; i++) {
                    target.bump();
                    target.bumpTwice();
                    bumpStatic();
                }
            });
            ts[t].start();
        }
        start.countDown();
        for (Thread t : ts) {
            t.join();
        }

        // bump() runs once directly and once via bumpTwice() per iteration.
        int expectedInstance = THREADS * ITERS * 2;
        int expectedReentrant = THREADS * ITERS;
        int expectedStatic = THREADS * ITERS;
        check(target.readInstance() == expectedInstance,
                "instance monitor lost updates: " + target.readInstance() + " != " + expectedInstance);
        check(target.reentrantCount == expectedReentrant,
                "re-entrant monitor lost updates: " + target.reentrantCount + " != " + expectedReentrant);
        check(staticCount == expectedStatic,
                "static monitor lost updates: " + staticCount + " != " + expectedStatic);

        // --- 4: the monitor is released when the method throws -------------
        // Hot enough to compile, so the compiled body's guard Drop is what is
        // being exercised. A leaked monitor makes the join below hang, which
        // the suite scores as a timeout rather than a silent pass.
        int caught = 0;
        for (int i = 0; i < ITERS; i++) {
            try {
                target.thrower();
            } catch (IllegalStateException expected) {
                caught++;
            }
        }
        check(caught == ITERS, "thrower did not throw every time: " + caught);
        Thread after = new Thread(() -> {
            for (int i = 0; i < 1000; i++) {
                target.bump();
            }
        });
        after.start();
        after.join(30_000);
        check(!after.isAlive(), "monitor leaked on the exception path (thread still blocked)");
        check(target.readInstance() == expectedInstance + 1000,
                "post-exception bumps lost: " + target.readInstance());

        // --- 5: the implicit monitor really is the receiver ----------------
        // Hold the receiver from outside; a synchronized method must not be
        // able to enter until we let go. If the compiled body locked nothing
        // (or locked the wrong object), the helper finishes early and the
        // observed value is already bumped when we look.
        final int before = target.readInstance();
        final int[] observed = new int[1];
        Thread contender;
        synchronized (target) {
            contender = new Thread(() -> {
                target.bump();
                synchronized (target) {
                    observed[0] = target.instanceCount;
                }
            });
            contender.start();
            // Give it a real chance to run and (incorrectly) get in.
            Thread.sleep(150);
            check(target.instanceCount == before,
                    "a synchronized method entered while the receiver was held: "
                            + target.instanceCount + " != " + before);
        }
        contender.join(30_000);
        check(!contender.isAlive(), "contender never acquired the receiver's monitor");
        check(observed[0] == before + 1, "contender saw " + observed[0] + ", expected " + (before + 1));

        System.out.println("CK RSyncMethodJit checks=" + checks);
        System.out.println("CK RSyncMethodJit counts=" + target.readInstance() + ","
                + target.reentrantCount + "," + staticCount + "," + caught);
        System.out.println("PASS RSyncMethodJit");
    }
}
