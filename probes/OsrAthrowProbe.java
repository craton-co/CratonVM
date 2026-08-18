/**
 * The RBC.6 lift's differential acceptance test: a once-called method whose hot
 * loop lives in a body containing a bare `athrow` (a `throw` with NO local
 * `catch`, so no exception table at all).
 *
 * Before the lift, `compile_osr_artifact` refused every such method outright on
 * `scan.has_athrow`, and OSR is the ONLY door out of the interpreter for a
 * method invoked once — so the loop ran interpreted for its whole life. That is
 * what made `BOBYQAOptimizerTest`'s `trsbox`/`bobyqb` (translated-from-Fortran
 * numerical code that `throw`s a `MathIllegalStateException` on an internal
 * assertion, never caught locally) effectively hang; see
 * `docs/known-issues/jit/bobyqa-hot-loop-refused-osr-because-of-a-bare-athrow-20260817.md`.
 *
 * The refusal guarded a real hazard, stated in its own comment: "the OSR bail
 * path resumes interpretation at the back-edge, so an athrow lowering that ran
 * side effects natively before throwing could see them re-applied". So the
 * arms below are not a throughput probe with a correctness footnote — the
 * side-effect COUNT is the primary measurement, and `takenThrow` is the arm
 * that can actually fail:
 *
 *   * `coldThrow`     — the BOBYQA shape. The `throw` is on a path never taken;
 *                       the loop must compile and produce the exact sum.
 *   * `takenThrow`    — the hazard shape. The `throw` fires ~60% into a hot
 *                       loop that increments a static counter every iteration.
 *                       The counter must read EXACTLY `throwAt + 1`. A stale
 *                       resume at the back-edge (the pre-lift bail path, and
 *                       the RBC.7 silent-corruption shape) re-runs every
 *                       iteration between OSR entry and the throw, so this
 *                       counter reads HIGH — with no exception anywhere.
 *   * `nestedThrow`   — the callee-unwind control: the throw happens in a
 *                       CALLEE, so this arm carries no `athrow` of its own and
 *                       its OSR admission did not change with the lift. Same
 *                       exact-count rule, on a shape the lift did not touch.
 *   * `control`       — the identical loop with the `throw` statement deleted,
 *                       so the throughput columns have a same-binary control
 *                       and "compiled" is not inferred from a wall-clock guess.
 *
 * Every arm is called exactly ONCE from `main`, which is the whole point: a
 * second call would open the method-entry tier-up door and the measurement
 * would no longer be about OSR.
 *
 *   javac -d . OsrAthrowProbe.java
 *   CRATONVM_DBG_JITC=1 cratonvm --java-home <jdk> -cp . OsrAthrowProbe 400000
 *     2>&1 | grep -E 'OSR-compile( FAILED)?'
 *
 * Exit status is 0 only when every count matches; the arms print PASS/FAIL
 * themselves so a run under a suite harness fails loudly rather than silently
 * reporting a fast wrong answer.
 */
public final class OsrAthrowProbe {
    static long sink;
    static int effects;
    static int failures;

    /** Pre-allocated so the throw path costs no allocation and no stack walk. */
    static final RuntimeException E = new RuntimeException("probe") {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };

    static int leaf(int i) { return i + 1; }

    /** Throws only when `i` is negative, which the callers never pass. */
    static void coldThrower(int i) { if (i < 0) { throw E; } }

    // ---- arms -------------------------------------------------------------

    /**
     * The BOBYQA shape: a bare `athrow` on a cold assertion path, no local
     * handler, hot loop around it. Must return the exact sum.
     */
    static long coldThrow(int n) {
        long a = 0;
        for (int i = 0; i < n; i++) {
            if (i < 0) { throw E; }   // bare athrow, never taken, no local catch
            a += leaf(i);
        }
        return a;
    }

    /** The control: byte-for-byte `coldThrow` with the `throw` deleted. */
    static long control(int n) {
        long a = 0;
        for (int i = 0; i < n; i++) {
            a += leaf(i);
        }
        return a;
    }

    /**
     * The hazard shape. Every iteration commits a side effect (`effects++`)
     * BEFORE the throw can fire, so the count is an exact witness of how many
     * iterations ran. Re-running committed iterations after the bail — what
     * RBC.6's comment feared — reads HIGH here and nowhere else.
     */
    static void takenThrow(int n, int throwAt) {
        for (int i = 0; i < n; i++) {
            effects++;
            sink += leaf(i);
            if (i == throwAt) { throw E; }   // bare athrow, no local catch
        }
    }

    /**
     * The callee-unwind control. This arm has NO `athrow` of its own (the
     * throw is in `coldThrower`), so it is the arm that was already admitted
     * before the lift — it holds the exact-count rule to a shape whose OSR
     * admission did not change, which is what makes a HIGH count in the
     * `athrow` arms attributable to the lift rather than to OSR in general.
     */
    static void nestedThrow(int n, int throwAt) {
        for (int i = 0; i < n; i++) {
            effects++;
            sink += leaf(i);
            coldThrower(i == throwAt ? -1 : i);
        }
    }

    // ---- harness ----------------------------------------------------------

    static void check(String what, long got, long want) {
        boolean ok = got == want;
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "PASS " : "FAIL ") + what + " got=" + got + " want=" + want);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        int throwAt = (int) (n * 0.6);
        long want = (long) n * (n + 1) / 2;   // sum of leaf(i) = i+1 for i in [0,n)

        long t0 = System.nanoTime();
        long cold = coldThrow(n);
        long t1 = System.nanoTime();
        long ctl = control(n);
        long t2 = System.nanoTime();

        check("coldThrow.sum", cold, want);
        check("control.sum", ctl, want);

        effects = 0;
        boolean caught = false;
        try {
            takenThrow(n, throwAt);
        } catch (RuntimeException e) {
            caught = e == E;
        }
        check("takenThrow.caught", caught ? 1 : 0, 1);
        check("takenThrow.effects", effects, throwAt + 1L);

        effects = 0;
        caught = false;
        try {
            nestedThrow(n, throwAt);
        } catch (RuntimeException e) {
            caught = e == E;
        }
        check("nestedThrow.caught", caught ? 1 : 0, 1);
        check("nestedThrow.effects", effects, throwAt + 1L);

        System.out.println("ns/iter  coldThrow=" + ((t1 - t0) / n) + "  control=" + ((t2 - t1) / n));
        System.out.println("sink=" + sink + " failures=" + failures);
        if (failures != 0) {
            System.exit(1);
        }
    }
}
