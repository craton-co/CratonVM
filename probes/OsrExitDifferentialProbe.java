/**
 * The OSR **exit** differential: does a forced mid-loop bail resume the
 * interpreter in the state the program says it is in?
 *
 * `docs/feature-designs/jit-osr-exit-and-recompile.md` step 3. The defect this
 * exists to catch is a *wrong-answer* bug, not a slowdown: if the bail point is
 * not the exact interpreter state the loop was in, the loop body executes more
 * times than the program says. Every test that only checks the method
 * terminates — and every test that only checks the final sum — is blind to it,
 * because the accumulators of a re-run iteration are usually re-derived from
 * the induction variable.
 *
 * <h2>Why the accumulator is what it is</h2>
 *
 * The discriminating observable is **how many times the loop body ran**, and
 * nothing else here is sufficient:
 *
 * <ul>
 *   <li>{@code execs} — a static counter incremented once per body execution.
 *       A re-run iteration increments it twice; a skipped one, not at all.
 *       Both the interpreter and compiled code commit this store, so it is a
 *       faithful count of executions rather than of iterations <em>intended</em>.
 *   <li>{@code trace} — an FNV-1a chain over the induction variable, mixed once
 *       per body execution. Order- and multiplicity-sensitive: a re-run of
 *       iteration <em>k</em> perturbs it even when {@code execs} would be
 *       restored by a compensating skip elsewhere.
 *   <li>the shape's own return value — the ordinary "did it compute the right
 *       answer" check, kept because a state transfer that corrupts a
 *       live-across local shows up here and nowhere else.
 * </ul>
 *
 * A shape whose result is a pure function of {@code n} (e.g. {@code sum += i})
 * is deliberately paired with {@code execs}/{@code trace}: the sum alone is the
 * weak check the lane's history records as having missed the defect.
 *
 * <h2>Constraints the shapes respect</h2>
 *
 * <ul>
 *   <li><b>No {@code try}/{@code catch} in a hot method.</b> {@code
 *       compile_osr_artifact} refuses (RBC.6b) a method with a non-empty
 *       exception table, so such a shape would silently never OSR and the arm
 *       would pass vacuously.
 *   <li><b>No string concatenation inside a loop.</b> An {@code invokedynamic}
 *       inside the loop records its own uncommon-trap snapshot at the same bci
 *       as the loop-boundary exit map, which is a different exit path; it is
 *       exercised deliberately by one shape (8), not accidentally by all of
 *       them.
 *   <li><b>Every shape's loop header must be the method's LOWEST-pc loop
 *       header</b> to be the one {@code CRATONVM_OSR_EXIT_AFTER} arms
 *       (the trigger site is {@code loops.iter().map(header).min()}). Shape 6
 *       is the deliberate exception — a nested loop, where the armed header is
 *       the OUTER one.
 * </ul>
 *
 * <h2>How to run it</h2>
 *
 * {@code regression-suite/perf/osr-exit-differential.sh} drives every arm and
 * diffs them. By hand, the two that matter:
 *
 * <pre>
 *   cratonvm -cp probes OsrExitDifferentialProbe            # default
 *   CRATONVM_DBG=osr-exit-after=64 cratonvm -cp probes OsrExitDifferentialProbe
 * </pre>
 *
 * Every line of output must be byte-identical across HotSpot, {@code --nojit},
 * the default JIT, and every forced-exit arm.
 */
public final class OsrExitDifferentialProbe {

    /** Per-shape execution counter — reset by {@link #begin}. */
    private static long execs;

    /** Per-shape FNV-1a chain over the induction variable. */
    private static long trace;

    /** FNV-1a 64 offset basis, over every shape's (execs, trace, result). */
    private static long acc = -3750763034362895579L;

    private static int N = 400_000;

    private static void begin() {
        execs = 0;
        trace = -3750763034362895579L;
    }

    /** Fold one shape's three observables into {@link #acc} and report them. */
    private static void report(String shape, long result) {
        System.out.println("shape=" + shape + " n=" + N + " execs=" + execs
                + " trace=" + trace + " result=" + result);
        for (int i = 0; i < shape.length(); i++) {
            acc = (acc ^ shape.charAt(i)) * 1099511628211L;
        }
        acc = (acc ^ execs) * 1099511628211L;
        acc = (acc ^ trace) * 1099511628211L;
        acc = (acc ^ result) * 1099511628211L;
    }

    // ------------------------------------------------------------------
    // 1. The canonical shape. `sum` is a pure function of `n`, so it is the
    //    check that DOES NOT discriminate; `execs` and `trace` beside it are
    //    the ones that do.
    // ------------------------------------------------------------------
    private static long intSum(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            execs++;
            trace = (trace ^ i) * 1099511628211L;
            sum += i;
        }
        return sum;
    }

    // ------------------------------------------------------------------
    // 2. A loop-carried dependence the induction variable cannot re-derive:
    //    `carry` depends on its own previous value, so resuming one iteration
    //    early or late is not recoverable by running the remaining trips.
    // ------------------------------------------------------------------
    private static long carriedDependence(int n) {
        long carry = 1;
        for (int i = 0; i < n; i++) {
            execs++;
            trace = (trace ^ i) * 1099511628211L;
            carry = carry * 6364136223846793005L + (i | 1);
            carry ^= carry >>> 29;
            // Mix the loop-CARRIED state, not just the induction variable:
            // `trace` is then a digest of the frame at every iteration
            // boundary, which is as close as a Java-level probe gets to the
            // brief's "compare the resumed frame against the frame an
            // un-compiled run would have had at the same iteration count".
            trace = (trace ^ carry) * 1099511628211L;
        }
        return carry;
    }

    // ------------------------------------------------------------------
    // 3. A HEAP side effect per iteration. Re-running an iteration commits the
    //    store a second time; the checksum over the array is what sees it.
    //    The array is small so every slot is written many times and the LAST
    //    writer of each slot is what the checksum reads — i.e. the observable
    //    is the sequence of executions, not their count alone.
    // ------------------------------------------------------------------
    private static long heapSideEffect(int n, int[] cells) {
        for (int i = 0; i < n; i++) {
            execs++;
            trace = (trace ^ i) * 1099511628211L;
            int slot = i & (cells.length - 1);
            cells[slot] = cells[slot] * 31 + i;
            trace = (trace ^ cells[slot]) * 1099511628211L;
        }
        long sum = 0;
        for (int i = 0; i < cells.length; i++) {
            sum = sum * 1000003L + cells[i];
        }
        return sum;
    }

    // ------------------------------------------------------------------
    // 4. Locals live ACROSS the loop, of three kinds — a reference, a
    //    category-2 long, and an int. A transfer that writes a slot from the
    //    wrong copy of the exit map corrupts one of these and nothing else.
    // ------------------------------------------------------------------
    private static long liveAcrossLocals(int n, String tag) {
        long wide = 0x0123_4567_89AB_CDEFL;
        int narrow = tag.length() * 7;
        long sum = 0;
        for (int i = 0; i < n; i++) {
            execs++;
            trace = (trace ^ i) * 1099511628211L;
            sum += (i & 15) + narrow;
        }
        return sum + wide + narrow + tag.charAt(0);
    }

    // ------------------------------------------------------------------
    // 5. A double accumulator — the XMM half of the same transfer. FP is
    //    order-sensitive by construction, so a re-run iteration is visible in
    //    the bits even when the integral shapes would round it away.
    // ------------------------------------------------------------------
    private static long doubleAccumulator(int n) {
        double sum = 0.0;
        for (int i = 0; i < n; i++) {
            execs++;
            trace = (trace ^ i) * 1099511628211L;
            sum = sum * 1.0000001 + (i % 97) * 0.5;
        }
        return Double.doubleToLongBits(sum);
    }

    // ------------------------------------------------------------------
    // 6. Nested loops. The armed trigger site is the OUTER header (lowest pc),
    //    so the bail happens with the inner loop's whole iteration space
    //    already committed for the completed outer trips.
    // ------------------------------------------------------------------
    private static long nestedLoops(int outer, int inner) {
        long total = 0;
        for (int o = 0; o < outer; o++) {
            execs++;
            trace = (trace ^ o) * 1099511628211L;
            long acc0 = 0;
            for (int i = 0; i < inner; i++) {
                // Counted too: the INNER loop is the hot one and OSRs on its
                // own, so a double-execution there has to be visible. `execs`
                // is a checksum over both levels, not a per-level count.
                execs++;
                trace = (trace ^ (i + 0x5bf0_3635L)) * 1099511628211L;
                acc0 += (i ^ o) & 255;
            }
            total = total * 31 + acc0;
        }
        return total;
    }

    // ------------------------------------------------------------------
    // 7. A `long` induction variable. The category-2 high half is the slot the
    //    entry metadata strips a register assignment for; the exit has to put
    //    the pair back the way the interpreter reads it.
    // ------------------------------------------------------------------
    private static long longInduction(int n) {
        long product = 1;
        for (long i = 1; i <= n; i++) {
            execs++;
            trace = (trace ^ i) * 1099511628211L;
            product = product * 3 + i;
            product ^= product >>> 31;
            trace = (trace ^ product) * 1099511628211L;
        }
        return product;
    }

    // ------------------------------------------------------------------
    // 8. A loop whose body contains an `invokedynamic` string concatenation.
    //    That site records its own uncommon-trap snapshot, so a bail here can
    //    leave through an exit map that is NOT the loop-boundary one — which
    //    is exactly the case `osr_exit_points` is cross-checked against.
    //    Short trip count: the concat allocates.
    // ------------------------------------------------------------------
    private static long concatInLoop(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            execs++;
            trace = (trace ^ i) * 1099511628211L;
            String s = "v" + i;
            h = h * 131 + s.length();
        }
        return h;
    }

    public static void main(String[] args) {
        if (args.length > 0) {
            N = Integer.parseInt(args[0]);
        }
        int[] cells = new int[64];

        // Two rounds. The first drives every loop past the back-edge OSR
        // threshold; the second re-enters an artifact that is already
        // published, which is the path a forced exit + re-entry cycles
        // through. Both rounds' observables are folded into `acc`, so a
        // divergence that only appears on re-entry is not averaged away.
        for (int round = 0; round < 2; round++) {
            begin();
            report("intSum", intSum(N));
            begin();
            report("carriedDependence", carriedDependence(N));
            begin();
            report("heapSideEffect", heapSideEffect(N, cells));
            begin();
            report("liveAcrossLocals", liveAcrossLocals(N, "osr-exit"));
            begin();
            report("doubleAccumulator", doubleAccumulator(N));
            begin();
            report("nestedLoops", nestedLoops(N / 512 + 2, 512));
            begin();
            report("longInduction", longInduction(N));
            begin();
            report("concatInLoop", concatInLoop(N / 40 + 1));
        }

        System.out.println("OsrExitDifferentialProbe acc=" + acc);
    }
}
