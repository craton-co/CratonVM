/**
 * Regression: a self-recursive method with BOTH a covered and an uncovered
 * self-call site must not let its own `catch` swallow a throw at an UNCOVERED
 * bci.
 *
 * The guard for
 * `docs/internal/retired/r10-selfrec-deep-handler-leaks-once-per-million-20260922-RETIRED-20260922.md`,
 * and the deterministic successor to that page's `recDeep(11, 4)` measurement.
 *
 * # Why a second entry point, and why it is the one that matters
 *
 * The page it closes measured `recDeep(11, 4)` — entry through the COVERED
 * branch — and found a wrong answer about once per 1.2 million calls. That rate
 * is not the defect's rate. It is the rate at which the interpreter/compiled
 * boundary happens to land on an activation whose stamped throw pc is the
 * UNCOVERED self-call site, which during steady state needs the tier-up window
 * to fall in the middle of a recursion.
 *
 * Enter through the uncovered branch instead — `recDeep(4, 4)`, where every
 * activation's self-call is outside the `try` — and the boundary lands there on
 * essentially every call. Same defect, same route, 19,930 of 20,000 calls
 * instead of 1 in 1,200,000. Measured on `dev` 5a6d5073b:
 *
 *   arm                      wrongCovered   wrongUncovered
 *   --nojit                            0                0
 *   CRATONVM_C2_ACCEPT=never           0           19,930
 *   default                            0           19,930
 *   CRATONVM_C2_ACCEPT=always          0           19,937
 *
 * `wrongCovered` is 0 on the pristine binary at this vector's call count, which
 * is why this file asserts the UNCOVERED column and carries the covered one as
 * a control rather than as evidence.
 *
 * # The shapes
 *
 *   * `uncoveredEntry` — the vector. `recDeep(4, 4)` must throw; the JIT
 *     returned `1004` from a `catch` whose protected range does not cover the
 *     throw site, in the wrong activation, with that activation's locals.
 *   * `coveredEntry`   — `recDeep(11, 4)`, the shape the retired page measured.
 *     Control at this call count.
 *   * `recCovered`     — a self-recursive method with ONLY a covered self-call
 *     site. The second control: it was green on both binaries, so if it ever
 *     goes red the defect is not this one.
 *
 * Every assertion reads a value produced inside `drive`, for the reason
 * `RJitSelfRecCatch`'s class doc gives: a value printed from `main` is a value
 * this VM never compiled.
 */
public class RJitSelfRecUncovered {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    /** Tuned against the negative control (pristine `dev`): red at 19,930/20,000. */
    private static final int WARM = 40;
    private static final int REPS = 500;

    static long recDeep(int n, int floor) {
        if (n <= 0) {
            throw new IllegalStateException("bottom");
        }
        if (n <= floor) {
            return recDeep(n - 1, floor);          // UNCOVERED self-call
        }
        try {
            return recDeep(n - 1, floor) + n;      // COVERED self-call
        } catch (IllegalStateException e) {
            return 1000 + n;
        }
    }

    /** `recDeep(d, d)` takes the uncovered branch at every level, so it throws. */
    static String uncoveredEntry(int depth) {
        try {
            return "RET " + recDeep(depth, depth);
        } catch (IllegalStateException e) {
            return "THREW";
        }
    }

    /** Only a covered self-call site: the second control. */
    static long recCovered(int n) {
        if (n <= 0) {
            throw new IllegalStateException("bottom");
        }
        try {
            return recCovered(n - 1) + n;
        } catch (IllegalStateException e) {
            return -n;
        }
    }

    // Counted INSIDE the loop.
    static int wrongUncovered, wrongCovered, wrongCtl;
    static String lastUncovered = "";
    static long lastCovered, lastCtl;

    static void drive(int reps) {
        for (int i = 0; i < reps; i++) {
            String s = uncoveredEntry(4);
            if (!s.equals("THREW")) { wrongUncovered++; lastUncovered = s; }
            long v = recDeep(11, 4);
            if (v != 1056) { wrongCovered++; lastCovered = v; }
            long c = recCovered(9);
            if (c != 43) { wrongCtl++; lastCtl = c; }
        }
    }

    public static void main(String[] args) {
        for (int w = 0; w < WARM; w++) {
            drive(REPS);
        }
        long calls = (long) WARM * REPS;

        check(wrongUncovered == 0, "uncoveredEntry(4): " + wrongUncovered + "/" + calls
                + " calls returned " + lastUncovered + ", want THREW"
                + ("RET 1004".equals(lastUncovered)
                        ? " (RET 1004 = the catch over [27,37) swallowed a throw at"
                          + " the uncovered bci 23, in the wrong activation)" : ""));
        check(wrongCovered == 0, "CONTROL recDeep(11,4): " + wrongCovered + "/" + calls
                + " calls returned " + lastCovered + ", want 1056");
        check(wrongCtl == 0, "CONTROL recCovered(9): " + wrongCtl + "/" + calls
                + " calls returned " + lastCtl + ", want 43"
                + " — a self-recursive method with ONLY a covered self-call site"
                + " was green on both binaries; if this is red the defect is not"
                + " the one this vector was written for");

        System.out.println("CK calls=" + calls + " uncovered=" + wrongUncovered
                + " covered=" + wrongCovered + " ctl=" + wrongCtl);
        System.out.println("PASS RJitSelfRecUncovered (" + checks + " checks)");
    }
}
