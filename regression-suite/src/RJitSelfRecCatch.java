/**
 * Regression: a self-recursive method that CATCHES what its own recursive call
 * throws must return what the bytecode says, in every JIT arm — and the answer
 * has to be read from INSIDE the compiled caller, not printed from `main`.
 *
 * The guard for
 * `docs/internal/retired/r10-self-recursive-catch-of-its-own-call-is-miscompiled-20260921-RETIRED-20260922.md`.
 *
 * # Why this vector exists rather than the probe it replaces
 *
 * `regression-suite/probes/R10SelfRecCatch.java` warmed the three shapes in a
 * hot `drive` loop and then printed four values — from `main`, which this VM
 * never compiles. So every column of that page's closing measurement read the
 * INTERPRETER, and the page was closed "6/6 oracle in every arm" while a
 * silent wrong answer was live in two of them. Measured on `dev` 7281ce4b6,
 * counting results inside the warm loop instead of after it: 9,941 of 10,000
 * calls wrong for the NPE shape, 9,940 for AIOOBE, 17–226 for the arithmetic
 * one. The root cause was `vm::jit::helpers::invoke_site_has_receiver` — a
 * `!= 3` receiver test that consumed argument 0 of a `invoke_kind == 4`
 * self-recursive STATIC direct call as `this`, shifting every local of the
 * rebuilt handler frame by one slot.
 *
 * Hence the rule this file is built around: **every assertion reads a value
 * produced inside `drive`.** `main` only prints the totals.
 *
 * # The shapes
 *
 * Four bottoms to one recursion, because the route differs by how the
 * exception is raised and only the last one was ever green:
 *
 *   * `recNpe`    — `NUL.hashCode()`, an implicit NPE;
 *   * `recAioobe` — `EMPTY[3]`, an implicit ArrayIndexOutOfBoundsException;
 *   * `recArith`  — `1 / 0`, an implicit ArithmeticException (the divide trap,
 *     the path that produces the `i64::MIN` deopt sentinel);
 *   * `recExpl`   — `throw new IllegalStateException`, an explicit throw. This
 *     is the CONTROL: it was correct in every arm of every measurement, before
 *     and after the fix, because an explicit throw does not reach the
 *     deopt-servicing door. If it ever goes red the defect is not the one this
 *     vector was written for.
 *
 * All four catch in the activation directly above the throw, so each must
 * return `-1 + 2 + 3 + ... + 9 == 43`. A handler run one activation off, or
 * run with the wrong activation's locals, returns 44 — the shape the fix
 * closed, and the reason the expected value is asserted rather than diffed
 * alone.
 *
 * `WARM`/`REPS` are tuned against a negative control (the pre-fix binary): at
 * 20x500 the NPE and AIOOBE shapes failed on ~99.4% of calls and the whole
 * vector failed 10/10 runs in the default arm, in ~0.9 s.
 */
public class RJitSelfRecCatch {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    /** Tuned against a negative control; see the class doc. */
    private static final int WARM = 20;
    private static final int REPS = 500;
    /** `-1 + 2 + 3 + ... + 9`. A handler one activation off returns 44. */
    private static final long WANT = 43;

    static final int[] EMPTY = new int[0];
    static Object NUL = null;

    static long recNpe(int n) {
        if (n <= 0) return NUL.hashCode();
        try { return recNpe(n - 1) + n; } catch (NullPointerException e) { return -n; }
    }

    static long recAioobe(int n) {
        if (n <= 0) return EMPTY[3];
        try { return recAioobe(n - 1) + n; } catch (ArrayIndexOutOfBoundsException e) { return -n; }
    }

    static long recArith(int n, int d) {
        if (n <= 0) return 1 / d;
        try { return recArith(n - 1, d) + n; } catch (ArithmeticException e) { return -n; }
    }

    static long recExpl(int n) {
        if (n <= 0) throw new IllegalStateException("bottom");
        try { return recExpl(n - 1) + n; } catch (IllegalStateException e) { return -n; }
    }

    // Counted INSIDE the loop. A value checked after the loop is a value
    // checked in `main`, which is the blindness this vector replaces.
    static int wrongNpe, wrongAioobe, wrongArith, wrongExpl;
    static long lastNpe, lastAioobe, lastArith, lastExpl;

    static void drive(int reps) {
        for (int i = 0; i < reps; i++) {
            long v;
            v = recNpe(9);      if (v != WANT) { wrongNpe++;    lastNpe = v; }
            v = recAioobe(9);   if (v != WANT) { wrongAioobe++; lastAioobe = v; }
            v = recArith(9, 0); if (v != WANT) { wrongArith++;  lastArith = v; }
            v = recExpl(9);     if (v != WANT) { wrongExpl++;   lastExpl = v; }
        }
    }

    public static void main(String[] args) {
        for (int w = 0; w < WARM; w++) {
            drive(REPS);
        }
        long calls = (long) WARM * REPS;

        check(wrongNpe == 0, "recNpe: " + wrongNpe + "/" + calls
                + " calls returned " + lastNpe + ", want " + WANT
                + (lastNpe == WANT + 1 ? " (44 = the handler ran in the throwing"
                        + " activation, with its locals)" : ""));
        check(wrongAioobe == 0, "recAioobe: " + wrongAioobe + "/" + calls
                + " calls returned " + lastAioobe + ", want " + WANT);
        check(wrongArith == 0, "recArith: " + wrongArith + "/" + calls
                + " calls returned " + lastArith + ", want " + WANT);
        check(wrongExpl == 0, "CONTROL recExpl: " + wrongExpl + "/" + calls
                + " calls returned " + lastExpl + ", want " + WANT
                + " — the explicit-throw shape does not reach the"
                + " deopt-servicing door and has never been red");

        System.out.println("CK calls=" + calls + " npe=" + wrongNpe
                + " aioobe=" + wrongAioobe + " arith=" + wrongArith
                + " explicit=" + wrongExpl);
        System.out.println("PASS RJitSelfRecCatch (" + checks + " checks)");
    }
}
