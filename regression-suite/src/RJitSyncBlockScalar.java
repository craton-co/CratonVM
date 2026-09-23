/**
 * Regression: a {@code synchronized} block over an allocation that never
 * escapes the method must keep behaving like a real lock over a real object —
 * including when an exception is thrown from inside it.
 *
 * <h2>What changed under this vector</h2>
 *
 * Before 2026-09-22 the single-pass x64 backend could not scalar-replace
 * ANYTHING in a method compiled with precise exception frames, and every javac
 * {@code synchronized} block sets that flag: javac must protect the body with a
 * catch-all handler that re-exits the monitor and rethrows,
 *
 * <pre>
 *     astore &lt;e&gt; ; aload &lt;mon&gt; ; monitorexit ; aload &lt;e&gt; ; athrow
 * </pre>
 *
 * and {@code <mon>} is a synthesised temporary allocated above every declared
 * parameter slot, so the handler always reads a non-parameter local. The
 * backend's whole monitor-relock feature was therefore unreachable — see
 * {@code docs/internal/fixed-bugs/} for the page that measured it.
 *
 * Three gates had to open together: the escape analysis had to model
 * {@code monitorenter}/{@code monitorexit} exactly, it had to walk through the
 * {@code goto} javac emits over that handler, and the driver had to stop
 * emptying the non-escaping set under precise exception frames. With all three
 * open, {@code hold()} below compiles with its allocation deleted, its
 * {@code <init>} skipped and BOTH monitor operations removed.
 *
 * <h2>Why this vector is about the EXCEPTION path</h2>
 *
 * Deleting an uncontended lock nobody can observe is the easy half. The half
 * that can be silently wrong is what happens when the body throws: the
 * compiled frame holds a dummy zero where the object was and holds no lock at
 * all, so resuming javac's handler needs the VM to rebuild the object and
 * re-acquire the lock it never took, or the {@code monitorexit} in that handler
 * runs against a lock nobody entered. Three outcomes are all wrong and none of
 * them crashes visibly:
 *
 * <ul>
 *   <li>the exception PROPAGATES past a {@code catch} that matches it —
 *       {@code throwsInside()} would not return its sentinel;</li>
 *   <li>the handler runs and raises {@code IllegalMonitorStateException}
 *       instead of rethrowing;</li>
 *   <li>the lock is left held, and the next acquirer of the same object
 *       deadlocks — which the re-entrant and cross-thread arms below detect.</li>
 * </ul>
 *
 * Every number printed is exact and compared against HotSpot by the harness,
 * so a wrong answer on any of them is a diff rather than a crash.
 */
public class RJitSyncBlockScalar {

    /** The shape escape analysis is asked about: allocated, locked, dropped. */
    static final class Cell {
        int x;
    }

    static int checks = 0;

    static void check(boolean ok, String what) {
        checks++;
        if (!ok) {
            throw new AssertionError("RJitSyncBlockScalar: " + what);
        }
    }

    /**
     * The whole point: {@code c} is allocated here, locked here and never
     * leaves. Hot enough to compile, and its result depends on every field
     * write made inside the monitor.
     */
    static int hold(int n) {
        Cell c = new Cell();
        synchronized (c) {
            c.x = n;
            c.x += 1;
        }
        return c.x;
    }

    /**
     * The same shape with a nested (re-entrant) lock on the same object. The
     * compiled body deletes both levels; a resume has to put both back, or the
     * outer {@code monitorexit} unbalances.
     */
    static int holdNested(int n) {
        Cell c = new Cell();
        synchronized (c) {
            synchronized (c) {
                c.x = n;
            }
            c.x += 2;
        }
        return c.x;
    }

    /**
     * Throws from INSIDE the monitor. Returns the sentinel only if the
     * {@code catch} was actually entered, so a propagated exception is a
     * missing line rather than a silent pass.
     */
    static int throwsInside(int n) {
        Cell c = new Cell();
        try {
            synchronized (c) {
                c.x = n;
                if (c.x >= 0) {
                    throw new IllegalStateException("from inside the monitor");
                }
                c.x = -1;
            }
        } catch (IllegalStateException e) {
            // Reached only through javac's monitor handler, which re-exits the
            // lock and rethrows. If the resume did not re-acquire the elided
            // lock, that exit raises IllegalMonitorStateException and this
            // catch never runs.
            return 1000 + c.x;
        }
        return -1;
    }

    /**
     * After an exception has unwound out of a {@code synchronized} block over
     * a scalar-replaced object, the object's monitor must be free. Locking it
     * again from the SAME thread proves the relock/exit balanced to zero: a
     * lock left one level deep is invisible to a re-entrant acquire, so the
     * cross-thread arm in {@code main} is what closes that gap.
     */
    static int reacquireAfterThrow(int n) {
        Cell c = new Cell();
        try {
            synchronized (c) {
                c.x = n;
                throw new IllegalStateException("x");
            }
        } catch (IllegalStateException e) {
            // ignored on purpose
        }
        synchronized (c) {
            c.x += 7;
        }
        return c.x;
    }

    /** Side effect of {@link #viaCallee}, so the callee cannot be folded away. */
    static int sink;

    /** Throws for even arguments, returns for odd ones. */
    static void boom(int i) {
        if ((i & 1) == 0) {
            throw new IllegalStateException("boom");
        }
        sink += 1;
    }

    /**
     * THE ARM THAT ACTUALLY REACHES THE REBUILD, and the reason the three above
     * are not enough.
     *
     * {@code throwsInside} has an {@code athrow} in the protected region, and
     * `athrow` is a hard control-transfer barrier in the single-pass escape
     * analysis: it escapes every object held in a local, so {@code c} is NOT
     * scalar-replaced there and the compiled body keeps its real allocation and
     * its real lock. That arm is a correctness control — it must keep working —
     * but it exercises none of the new machinery.
     *
     * Here the throw comes out of a CALL instead. An invoke escapes the tracked
     * operand stack and leaves locals alone, so {@code c} stays non-escaping,
     * its allocation and both monitor operations are deleted, and an exception
     * from {@code boom} leaves compiled code through the reason-9 route with a
     * frame that names an object that was never allocated and a lock that was
     * never taken. Measured on this vector, 2026-09-22:
     *
     * <pre>
     *   scalar_replaced=1 monitor_elided_pcs=[11, 33]
     *   monitor_at_pcs=[12, 13, 14, 17, 18, 21, 22, 23, 26, 28, 29, 32, 33]
     *   MATERIALIZE bci=18 objects=1
     * </pre>
     *
     * Both halves have to be right or the answer changes: without the rebuild
     * the handler reads a null {@code c}, and without the relock javac's
     * monitor handler raises {@code IllegalMonitorStateException} instead of
     * rethrowing. The expected value below distinguishes every outcome.
     */
    static int viaCallee(int n) {
        Cell c = new Cell();
        try {
            synchronized (c) {
                c.x = n;
                boom(n);
                c.x += 100;
            }
        } catch (IllegalStateException e) {
            return 1000 + c.x;
        }
        return c.x;
    }

    public static void main(String[] args) throws Exception {
        // Warm every shape well past any tier threshold, and assert on every
        // iteration rather than only the last: a body that becomes wrong the
        // moment it is compiled must not be able to hide behind a cold check.
        int sum = 0;
        for (int i = 0; i < 200_000; i++) {
            int h = hold(i);
            if (h != i + 1) {
                throw new AssertionError("hold(" + i + ") = " + h);
            }
            sum += h & 1;
        }
        check(sum == 100_000, "hold parity sum = " + sum);

        int nested = 0;
        for (int i = 0; i < 200_000; i++) {
            int h = holdNested(i);
            if (h != i + 2) {
                throw new AssertionError("holdNested(" + i + ") = " + h);
            }
            nested += h & 1;
        }
        check(nested == 100_000, "holdNested parity sum = " + nested);

        int caught = 0;
        // A tenth of the loops above, and the asymmetry is deliberate: every
        // iteration here leaves compiled code through the reason-9 route and
        // finishes in the interpreter, which is ~100x the cost of the
        // straight-line arms. 20_000 is far past every tier threshold this VM
        // has, so the arm still measures the compiled body.
        for (int i = 0; i < 20_000; i++) {
            int r = throwsInside(i);
            if (r != 1000 + i) {
                throw new AssertionError("throwsInside(" + i + ") = " + r
                        + " (expected " + (1000 + i) + ")");
            }
            caught++;
        }
        check(caught == 20_000, "throwsInside entered its catch " + caught + " times");

        int reacquired = 0;
        for (int i = 0; i < 20_000; i++) {
            int r = reacquireAfterThrow(i);
            if (r != i + 7) {
                throw new AssertionError("reacquireAfterThrow(" + i + ") = " + r);
            }
            reacquired++;
        }
        check(reacquired == 20_000, "reacquireAfterThrow ran " + reacquired + " times");

        // The rebuild arm. Half the iterations throw out of the callee and take
        // the reason-9 route; the other half return normally through the
        // compiled body with its allocation and both locks deleted. Both
        // answers are checked on every iteration, so an arm that is right only
        // when it deopts (or only when it does not) fails here rather than
        // averaging out.
        long viaAcc = 0;
        for (int i = 0; i < 100_000; i++) {
            int r = viaCallee(i);
            int expect = ((i & 1) == 0) ? 1000 + i : i + 100;
            if (r != expect) {
                throw new AssertionError("viaCallee(" + i + ") = " + r
                        + " (expected " + expect + ")");
            }
            viaAcc += r;
        }
        check(viaAcc == 5_054_950_000L, "viaCallee accumulator = " + viaAcc);
        check(sink == 50_000, "viaCallee callee ran to completion " + sink + " times");

        // ── The cross-thread arm ────────────────────────────────────────────
        //
        // A monitor left held by a compiled body is invisible to the same
        // thread (its next acquire is re-entrant and succeeds). Only another
        // thread can see it, and only if it can name the same object — so this
        // arm uses a SHARED object, which escapes and is therefore NOT
        // scalar-replaced. That is deliberate: it is the control that says the
        // suite's monitor plumbing still works for a real lock, and it is what
        // would hang if the elided-lock relock leaked a level.
        final Object shared = new Object();
        final int[] observed = new int[1];
        Thread t;
        synchronized (shared) {
            t = new Thread(() -> {
                synchronized (shared) {
                    observed[0] = 42;
                }
            });
            t.start();
            Thread.sleep(150);
            check(observed[0] == 0, "another thread entered a held monitor");
        }
        t.join(30_000);
        check(!t.isAlive(), "contender never acquired the shared monitor");
        check(observed[0] == 42, "contender saw " + observed[0]);

        System.out.println("CK RJitSyncBlockScalar hold=" + hold(41));
        System.out.println("CK RJitSyncBlockScalar holdNested=" + holdNested(40));
        System.out.println("CK RJitSyncBlockScalar throwsInside=" + throwsInside(5));
        System.out.println("CK RJitSyncBlockScalar reacquireAfterThrow=" + reacquireAfterThrow(3));
        System.out.println("CK RJitSyncBlockScalar viaCallee=" + viaCallee(8) + ","
                + viaCallee(9));
        System.out.println("CK RJitSyncBlockScalar viaAcc=" + viaAcc + " sink=" + sink);
        System.out.println("CK RJitSyncBlockScalar sums=" + sum + "," + nested + ","
                + caught + "," + reacquired);
        System.out.println("CK RJitSyncBlockScalar checks=" + checks);
        System.out.println("PASS RJitSyncBlockScalar");
    }
}
