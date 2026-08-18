/**
 * Does a compiled callee that declares its OWN exception table still run that
 * table when the caller reaches it through the machine-code inline cache?
 *
 * This is the acceptance test for lifting `mic_publish_exception_table_callees`
 * (`CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH`). The ban exists because the inline
 * MIC/PIC cascade in `jit/src/x64.rs` CALLs the cached entry directly, with no
 * Rust frame in between to notice the callee's `i64::MIN` trap sentinel and
 * route it through the callee's own handler.
 *
 *   java     CalleeExceptionTableSemanticsProbe                       -- the oracle
 *   cratonvm CalleeExceptionTableSemanticsProbe                       -- bar kept
 *   CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=1 cratonvm ...                 -- bar lifted
 *   CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=1 CRATONVM_JIT_SP_IC_DEOPT_CHECK=0 cratonvm ...
 *
 * The last line is the RED PROOF. `SP_IC_DEOPT_CHECK=0` deletes
 * `emit_inline_callee_deopt_check` -- the one instruction sequence that lets a
 * raw machine-code CALL notice the callee trapped -- so with the bar lifted it
 * removes exactly the mechanism this probe exists to test.
 *
 * **Which binary that arm must be run on matters, and the answer is
 * counter-intuitive.** On a binary BEFORE the interlock landed it fails 8 runs
 * out of 8: an `ArithmeticException` escapes `Div.apply`'s own `catch` and
 * reaches `main`. On a binary WITH the interlock it PASSES 8 out of 8 -- not
 * because the probe went blind, but because
 * `mic_publish_exception_table_callees` refuses to publish at all when the
 * check is not being emitted, so the unsound state is no longer reachable by
 * setting one switch. Confirm which of the two you are looking at with
 * `NativeFunnelFloorProbe`'s try/catch rung under the same environment: ~200
 * ns/op means nothing was published (interlocked), ~24 ns/op means it was
 * published with the check deleted, which is the state that fails here.
 *
 * A first version of this probe put the throwing call AFTER the warm-up loop
 * instead of inside it; that call ran from an interpreted frame, so every arm
 * passed and the probe proved nothing. Every throwing call below therefore
 * happens INSIDE the hot loop, at the same call site the warm-up published the
 * cache for.
 */
public final class CalleeExceptionTableSemanticsProbe {

    interface Op { int apply(int i); }

    static int bodyRuns;
    static int BAD;

    /** Implicit AIOOBE, caught by the callee's own table. */
    static final class Bounds implements Op {
        static final int[] A = { 10, 11, 12, 13 };
        @Override public int apply(int i) {
            bodyRuns++;
            try { return A[i]; }
            catch (ArrayIndexOutOfBoundsException e) { return -1; }
        }
    }

    /** Implicit NPE, caught by the callee's own table. */
    static final class Nulls implements Op {
        static int[] live = { 5 };
        @Override public int apply(int i) {
            bodyRuns++;
            int[] a = (i < 0) ? null : live;
            try { return a[0]; }
            catch (NullPointerException e) { return -2; }
        }
    }

    /** Implicit ArithmeticException, caught by the callee's own table. */
    static final class Div implements Op {
        @Override public int apply(int i) {
            bodyRuns++;
            try { return 100 / i; }
            catch (ArithmeticException e) { return -3; }
        }
    }

    /** An explicit athrow caught locally. */
    static final class Thrown implements Op {
        @Override public int apply(int i) {
            bodyRuns++;
            try {
                if (i < 0) { throw new IllegalStateException("x"); }
                return i;
            } catch (IllegalStateException e) { return -4; }
        }
    }

    /**
     * The callee declares a table that does NOT cover what it throws, so the
     * exception must reach the CALLER's handler -- not be swallowed, and not be
     * mis-attributed to the caller's own deopt.
     */
    static final class Uncovered implements Op {
        static final int[] A = { 1 };
        @Override public int apply(int i) {
            bodyRuns++;
            try { return A[i]; }
            catch (NullPointerException e) { return -5; }   // never matches AIOOBE
        }
    }

    /** A `finally` that must run on the exceptional path too. */
    static final class Finally implements Op {
        static int finallyRuns;
        static final int[] A = { 1 };
        @Override public int apply(int i) {
            bodyRuns++;
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
     * One in every four calls throws inside the callee. The loop, the call site
     * and the cache are the same on every iteration, so once the caller is
     * compiled the throwing iterations go through the published inline cache.
     */
    static long driveCaught(Op op, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += op.apply((i & 3) == 3 ? BAD : (i & 3)); }
        return acc;
    }

    /** The uncovered arm: the caller catches, inside the same hot loop. */
    static long driveUncovered(Op op, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try { acc += op.apply((i & 3) == 3 ? 7 : 0); }
            catch (ArrayIndexOutOfBoundsException e) { acc += 1000; }
        }
        return acc;
    }

    /**
     * `perRound` is the value one round of `n` iterations must accumulate to;
     * it is a fixed function of the callee's Java semantics, so a VM that
     * swallows, duplicates or mis-routes even one exception cannot match it.
     * Both the smallest and the largest round are checked, so one bad round
     * among many cannot average away.
     */
    static void arm(String name, Op op, int bad, int n, int rounds, long perRound) {
        BAD = bad;
        long worstAcc = Long.MIN_VALUE, bestAcc = Long.MAX_VALUE;
        long worstRuns = Long.MIN_VALUE, bestRuns = Long.MAX_VALUE;
        for (int r = 0; r < rounds; r++) {
            bodyRuns = 0;
            long acc = driveCaught(op, n);
            worstAcc = Math.max(worstAcc, acc); bestAcc = Math.min(bestAcc, acc);
            worstRuns = Math.max(worstRuns, bodyRuns); bestRuns = Math.min(bestRuns, bodyRuns);
        }
        check(name + ": min round accumulator", bestAcc, perRound);
        check(name + ": max round accumulator", worstAcc, perRound);
        check(name + ": min round body runs", bestRuns, n);
        check(name + ": max round body runs", worstRuns, n);
    }

    public static void main(String[] args) {
        int n      = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 12;
        n = (n / 4) * 4;
        int q = n / 4;

        // 10 + 11 + 12 + (-1) per group of four.
        arm("Bounds", new Bounds(), 99, n, rounds, (long) q * (10 + 11 + 12 - 1));
        // 5 + 5 + 5 + (-2); indices 0,1,2 are all >= 0 so all read live[0].
        arm("Nulls", new Nulls(), -1, n, rounds, (long) q * (5 + 5 + 5 - 2));
        // i&3==0 divides by zero too: -3 + 100 + 50 + (-3) with BAD == 0.
        arm("Div", new Div(), 0, n, rounds, (long) q * (-3 + 100 + 50 - 3));
        // 0 + 1 + 2 + (-4)
        arm("Thrown", new Thrown(), -1, n, rounds, (long) q * (0 + 1 + 2 - 4));

        // Uncovered: 1 + 1 + 1 + 1000 per group of four, the 1000 proving the
        // CALLER's catch is what ran.
        Op u = new Uncovered();
        long uBest = Long.MAX_VALUE, uWorst = Long.MIN_VALUE;
        long uRunsBest = Long.MAX_VALUE, uRunsWorst = Long.MIN_VALUE;
        for (int r = 0; r < rounds; r++) {
            bodyRuns = 0;
            long acc = driveUncovered(u, n);
            uBest = Math.min(uBest, acc); uWorst = Math.max(uWorst, acc);
            uRunsBest = Math.min(uRunsBest, bodyRuns); uRunsWorst = Math.max(uRunsWorst, bodyRuns);
        }
        check("Uncovered: min round accumulator", uBest, (long) q * (1 + 1 + 1 + 1000));
        check("Uncovered: max round accumulator", uWorst, (long) q * (1 + 1 + 1 + 1000));
        check("Uncovered: min round body runs", uRunsBest, n);
        check("Uncovered: max round body runs", uRunsWorst, n);

        // Finally: `A` has length 1, so only i&3 == 0 reads in range; the other
        // three indices (1, 2 and BAD == 9) all throw and are caught by the
        // callee, giving 1 + (-6) + (-6) + (-6) per group of four. The finally
        // block runs on every call, thrown or not.
        Op f = new Finally();
        BAD = 9;
        long fBest = Long.MAX_VALUE, fWorst = Long.MIN_VALUE;
        long finBest = Long.MAX_VALUE, finWorst = Long.MIN_VALUE;
        for (int r = 0; r < rounds; r++) {
            bodyRuns = 0; Finally.finallyRuns = 0;
            long acc = driveCaught(f, n);
            fBest = Math.min(fBest, acc); fWorst = Math.max(fWorst, acc);
            finBest = Math.min(finBest, Finally.finallyRuns);
            finWorst = Math.max(finWorst, Finally.finallyRuns);
        }
        check("Finally: min round accumulator", fBest, (long) q * (1 - 6 - 6 - 6));
        check("Finally: max round accumulator", fWorst, (long) q * (1 - 6 - 6 - 6));
        check("Finally: min round finally runs", finBest, n);
        check("Finally: max round finally runs", finWorst, n);

        System.out.println(fails == 0 ? "PROBE PASS" : ("PROBE FAIL fails=" + fails));
        if (fails != 0) { System.exit(1); }
    }
}
