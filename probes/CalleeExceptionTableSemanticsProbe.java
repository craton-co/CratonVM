/**
 * Does a compiled callee that declares its OWN exception table still run that
 * table when the caller reaches it through the machine-code inline cache?
 *
 * This is the acceptance test for lifting `mic_publish_exception_table_callees`
 * (`CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH`). The ban exists because the inline
 * MIC/PIC cascade in `jit/src/x64.rs` CALLs the cached entry directly, with no
 * Rust frame in between to notice the callee's `i64::MIN` trap sentinel and
 * route it through the callee's own handler. If that is still true, every arm
 * below returns the WRONG value with the ban lifted, so this probe is red on a
 * broken build rather than merely slower.
 *
 * Run it BOTH ways on one binary, and against HotSpot, which is the oracle:
 *
 *   java                                        CalleeExceptionTableSemanticsProbe
 *   cratonvm ...                                CalleeExceptionTableSemanticsProbe
 *   CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=1 cratonvm ... CalleeExceptionTableSemanticsProbe
 *
 * Every arm is reached through an INTERFACE call on a single implementation,
 * which is the shape that populates the monomorphic inline cache; a statically
 * bound call would be direct-bound at compile time and never exercise it. The
 * warm-up loop runs the NON-throwing input so the site is compiled, cached and
 * published before the throwing input arrives — the cache must already hold the
 * callee when the exception happens, or the arm silently tests the cold path.
 */
public final class CalleeExceptionTableSemanticsProbe {

    interface Op { int apply(int i); }

    static int sideEffects;

    /** Implicit AIOOBE, caught by the callee's own table. */
    static final class Bounds implements Op {
        static final int[] A = { 10, 11, 12, 13 };
        @Override public int apply(int i) {
            sideEffects++;                    // exactly ONE per call, thrown or not
            try { return A[i]; }
            catch (ArrayIndexOutOfBoundsException e) { return -1; }
        }
    }

    /** Implicit NPE, caught by the callee's own table. */
    static final class Nulls implements Op {
        static int[] live = { 5 };
        @Override public int apply(int i) {
            sideEffects++;
            int[] a = (i < 0) ? null : live;
            try { return a[0]; }
            catch (NullPointerException e) { return -2; }
        }
    }

    /** Implicit ArithmeticException, caught by the callee's own table. */
    static final class Div implements Op {
        @Override public int apply(int i) {
            sideEffects++;
            try { return 100 / i; }
            catch (ArithmeticException e) { return -3; }
        }
    }

    /** An explicit athrow caught locally. */
    static final class Thrown implements Op {
        @Override public int apply(int i) {
            sideEffects++;
            try {
                if (i < 0) { throw new IllegalStateException("x"); }
                return i;
            } catch (IllegalStateException e) { return -4; }
        }
    }

    /**
     * The callee declares a table that does NOT cover the exception it throws.
     * It must propagate to the CALLER's handler, not be swallowed here and not
     * be mis-attributed to the caller's own deopt.
     */
    static final class Uncovered implements Op {
        static final int[] A = { 1 };
        @Override public int apply(int i) {
            sideEffects++;
            try { return A[i]; }
            catch (NullPointerException e) { return -5; }   // never matches AIOOBE
        }
    }

    /** A `finally` that must run on the exceptional path too. */
    static final class Finally implements Op {
        static int finallyRuns;
        static final int[] A = { 1 };
        @Override public int apply(int i) {
            sideEffects++;
            try { return A[i]; }
            catch (ArrayIndexOutOfBoundsException e) { return -6; }
            finally { finallyRuns++; }
        }
    }

    static int fails;

    static void check(String what, long got, long want) {
        boolean ok = got == want;
        if (!ok) { fails++; }
        System.out.printf("%-46s %-4s got=%d want=%d%n", what, ok ? "OK" : "FAIL", got, want);
    }

    /**
     * Warm `op` on `goodInput` until the site compiles and its inline cache
     * publishes, then run `badInput` once and report the result and how many
     * times the callee body actually ran.
     */
    static long[] warmThenThrow(Op op, int goodInput, int badInput, int warm) {
        long acc = 0;
        for (int i = 0; i < warm; i++) { acc += op.apply(goodInput); }
        sideEffects = 0;
        int r = op.apply(badInput);
        return new long[] { r, sideEffects, acc };
    }

    public static void main(String[] args) {
        int warm = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;

        long[] b = warmThenThrow(new Bounds(), 1, 99, warm);
        check("Bounds: callee catch returns -1", b[0], -1);
        check("Bounds: callee body ran exactly once", b[1], 1);

        long[] n = warmThenThrow(new Nulls(), 1, -1, warm);
        check("Nulls: callee catch returns -2", n[0], -2);
        check("Nulls: callee body ran exactly once", n[1], 1);

        long[] d = warmThenThrow(new Div(), 5, 0, warm);
        check("Div: callee catch returns -3", d[0], -3);
        check("Div: callee body ran exactly once", d[1], 1);

        long[] t = warmThenThrow(new Thrown(), 3, -1, warm);
        check("Thrown: callee catch returns -4", t[0], -4);
        check("Thrown: callee body ran exactly once", t[1], 1);

        // Uncovered: the caller must see the AIOOBE.
        Op u = new Uncovered();
        long uacc = 0;
        for (int i = 0; i < warm; i++) { uacc += u.apply(0); }
        sideEffects = 0;
        int caught = 0;
        try { u.apply(7); }
        catch (ArrayIndexOutOfBoundsException e) { caught = 1; }
        catch (Throwable e) { caught = -100; }
        check("Uncovered: propagates AIOOBE to caller", caught, 1);
        check("Uncovered: callee body ran exactly once", sideEffects, 1);

        Op f = new Finally();
        long facc = 0;
        for (int i = 0; i < warm; i++) { facc += f.apply(0); }
        Finally.finallyRuns = 0;
        sideEffects = 0;
        int fr = f.apply(9);
        check("Finally: callee catch returns -6", fr, -6);
        check("Finally: finally ran exactly once", Finally.finallyRuns, 1);
        check("Finally: callee body ran exactly once", sideEffects, 1);

        // The warm loops must have produced the ordinary (non-throwing) values,
        // or the arms above measured a body that was never really exercised.
        check("warm accumulators non-degenerate", (uacc == warm && facc == warm) ? 1 : 0, 1);
        check("Bounds warm accumulator", b[2], 11L * warm);

        System.out.println(fails == 0 ? "PROBE PASS" : ("PROBE FAIL fails=" + fails));
        if (fails != 0) { System.exit(1); }
    }
}
