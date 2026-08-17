/**
 * Does an OSR'd method with an exception table still CATCH, and does it catch
 * with the right values?
 *
 * The acceptance test for the RBC.6b lift
 * (`docs/known-issues/jit/osr-refuses-any-method-with-an-exception-table-20260817.md`).
 * Until 2026-08-17 `compile_osr_artifact` refused every method with a non-empty
 * exception table, so this file's shapes could only ever run interpreted and
 * every arm below was trivially correct. The lift compiles them, which puts
 * three separable things at risk — and each arm here isolates one:
 *
 *   catchCount   — does the `catch` still run at all? This is the hazard RBC.6b
 *                  was written for (a servlet's
 *                  `try { resp.resetBuffer(); } catch (IllegalStateException)`
 *                  silently ceasing to catch). An OSR body whose protected-range
 *                  invoke does not publish a reason-9 frame propagates past its
 *                  own handler, so this arm reads 0.
 *
 *   handlerLocal — does the handler see the loop's CURRENT locals? The live
 *                  interpreter frame is parked at the pre-OSR back-edge pc with
 *                  the locals the compiled code never advanced, so a handler
 *                  entered on those reads a stale induction variable. Summing
 *                  `i` inside the catch makes that visible as a wrong number
 *                  rather than as nothing at all.
 *
 *   sideEffects  — are committed iterations re-run? `bodyRuns` and
 *                  `sideEffects` count one per iteration, so resuming at the
 *                  stale pc after a throw — which re-executes every iteration
 *                  since OSR entry (RBC.7) — shows up as a count larger than
 *                  `n` rather than as nothing at all.
 *
 *   escaped      — does an exception with NO handler still propagate? The lift
 *                  must not turn "outside every protected range" into a catch.
 *
 * All four numbers are deterministic functions of `n`, so this is a differential
 * probe: HotSpot, `--nojit`, CratonVM default, and CratonVM with
 * `CRATONVM_JIT_OSR_EXC_TABLE=0` must print identical lines.
 *
 *   javac -nowarn -d . probes/OsrExcTableProbe.java
 *   java              -cp . OsrExcTableProbe 300000     # the control
 *   cratonvm --java-home <jdk> -cp . OsrExcTableProbe 300000
 *   CRATONVM_JIT_OSR_EXC_TABLE=0 cratonvm --java-home <jdk> -cp . OsrExcTableProbe 300000
 *   cratonvm --java-home <jdk> --nojit -cp . OsrExcTableProbe 300000
 */
public final class OsrExcTableProbe {

    /** Pre-allocated so the throw itself is not an allocation benchmark. */
    static final IllegalStateException ISE = new IllegalStateException("probe") {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };
    static final IllegalArgumentException IAE = new IllegalArgumentException("probe") {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };

    static long catchCount;
    static long handlerLocalSum;
    static long sideEffects;
    static long escaped;
    static long bodyRuns;

    /** Throws on every 4096th value — a callee, so the throw crosses a real invoke. */
    static void mayThrow(int i) {
        if ((i & 0xFFF) == 0) {
            throw ISE;
        }
    }

    /** The escape arm's thrower — see `loopThrowEscapes`. */
    static void lateThrow(int i, int at) {
        if (i == at) {
            throw IAE;
        }
    }

    static void otherThrow(int i) {
        if ((i & 0x3FFF) == 0) {
            throw IAE;
        }
    }

    /**
     * The RBC.6b shape: once-invoked, hot loop, one invoke inside a `try`.
     *
     * The catch reads `i` — the loop's induction variable, a non-parameter
     * local — which is what makes a stale-frame handler entry visible.
     */
    static void loopWithCatch(int n) {
        int i = 0;
        do {
            bodyRuns++;
            try {
                mayThrow(i);
            } catch (IllegalStateException e) {
                catchCount++;
                handlerLocalSum += i;
            }
            sideEffects++;
            i++;
        } while (i != n);
    }

    /**
     * Two disjoint protected ranges, and a TYPED handler that must not catch the
     * other range's exception. A handler search run at the wrong pc — the
     * back-edge OSR entry pc, which is what the three implicit-exception drains
     * used before this lift — matches by class alone and would swallow one.
     */
    static void loopTwoRanges(int n) {
        int i = 0;
        do {
            try {
                mayThrow(i);
            } catch (IllegalStateException e) {
                catchCount++;
                handlerLocalSum += i;
            }
            try {
                otherThrow(i);
            } catch (IllegalArgumentException e) {
                catchCount += 1000;
                handlerLocalSum += i;
            }
            i++;
        } while (i != n);
    }

    /**
     * A throw from OUTSIDE every protected range of the OSR'd method: the
     * `catch` covers the `mayThrow` call only, and the `throw IAE` sits after
     * it. The exception must escape `loopThrowEscapes` and be caught by the
     * caller — the lift must not widen a handler to cover a site it does not.
     *
     * This is also the arm that exercises the router's "no precise frame ⇒ the
     * throw site is outside every protected range ⇒ propagate" deduction.
     */
    static void loopThrowEscapes(int n, int throwAt) {
        int i = 0;
        do {
            try {
                mayThrow(i);
            } catch (IllegalStateException e) {
                catchCount++;
            }
            // Through a CALLEE, not a bare `throw`: an `athrow` in the method
            // body is refused by RBC.6, a separate and still-standing gate, and
            // an arm that never compiles tests the interpreter rather than the
            // lift. (Measured: with `throw IAE` written inline here this method
            // read `OSR-compile FAILED` while its two siblings compiled.)
            lateThrow(i, throwAt);
            i++;
        } while (i != n);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;

        loopWithCatch(n);
        System.out.println("loopWithCatch  catchCount=" + catchCount
                + " handlerLocalSum=" + handlerLocalSum
                + " sideEffects=" + sideEffects
                + " bodyRuns=" + bodyRuns);

        catchCount = 0; handlerLocalSum = 0;
        loopTwoRanges(n);
        System.out.println("loopTwoRanges  catchCount=" + catchCount
                + " handlerLocalSum=" + handlerLocalSum);

        catchCount = 0; handlerLocalSum = 0;
        // `throwAt` deep into the loop, so OSR has certainly engaged and
        // committed iterations before the escaping throw. A throw in the first
        // few hundred iterations would exercise the interpreter, not the lift.
        try {
            loopThrowEscapes(n, n - (n / 8));
        } catch (IllegalArgumentException e) {
            escaped++;
        }
        System.out.println("loopEscapes    catchCount=" + catchCount
                + " escaped=" + escaped);
        // `otherThrow` is used by `loopTwoRanges` only; referenced here so a
        // future edit cannot quietly orphan it.
        if (args.length > 99) { otherThrow(0); }
    }
}
