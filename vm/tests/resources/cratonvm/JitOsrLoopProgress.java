package cratonvm;

/**
 * Regression fixture for the OSR bail that DISCARDED committed loop progress
 * (the retired jit-osr-bail-on-callee-exception-reruns-loop-iterations write-up).
 *
 * <p>Shape matters in three ways, and every entry point here keeps all three:
 *
 * <ul>
 *   <li>the loop lives in a CALLED static method, so it OSRs — with the loop
 *       directly in {@code main} the counters come out right, so any repro that
 *       inlines it looks clean;</li>
 *   <li>that method has NO exception table, which is also what
 *       {@code compile_osr_artifact} requires (RBC.6b) — so an exception
 *       reaching the OSR return always escapes the OSR'd method;</li>
 *   <li>the loop body commits a per-iteration side effect ({@code iters},
 *       {@code visits[i]}) BEFORE the throwing operation, so a re-run is
 *       observable.</li>
 * </ul>
 *
 * <p>Every entry point returns a MISMATCH COUNT (0 == pass), never a checksum: a
 * per-iteration visit tally names a duplicated iteration directly instead of
 * silently corrupting an expected value.
 */
public class JitOsrLoopProgress {

    static final int N = 20000;
    /** The iteration whose operation throws in the escape/NPE/AIOOBE shapes. */
    static final int TRIP = 12345;

    static class Boom extends RuntimeException {
        Boom(String m) {
            super(m);
        }
    }

    static final class Cell {
        final int v;

        Cell(int v) {
            this.v = v;
        }
    }

    static int iters;
    static int wrong;
    static int[] visits;

    private static void reset() {
        iters = 0;
        wrong = 0;
        visits = new int[N];
    }

    /**
     * Every iteration in {@code [0, ranTo]} must have run EXACTLY once, no
     * iteration past it may have run at all, and the exception must have been
     * caught by the loop method's caller.
     */
    private static int tally(int ranTo, boolean caught) {
        int bad = wrong + (caught ? 0 : 1);
        if (iters != ranTo + 1) {
            bad++;
        }
        for (int i = 0; i < N; i++) {
            int want = (i <= ranTo) ? 1 : 0;
            if (visits[i] != want) {
                bad++;
            }
        }
        return bad;
    }

    // ---------------------------------------------------------------
    // Shape 1 — the callee catches its own exception (the doc's repro).
    // ---------------------------------------------------------------

    static void maybeThrow(boolean fail) {
        if (fail) {
            throw new Boom("boom");
        }
    }

    /** Catches its own Boom; the handler reads only parameters. */
    static int step(int i, boolean fail) {
        try {
            maybeThrow(fail);
        } catch (Boom b) {
            return i * 2;
        }
        return i;
    }

    static int runCaught(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            iters++;
            visits[i]++;
            int v = i % 97;
            boolean fail = (i % 3) == 0;
            int got = step(v, fail);
            if (got != (fail ? v * 2 : v)) {
                wrong++;
            }
            c = c * 31 + got;
        }
        return c;
    }

    /**
     * The OSR tier used to bake a direct machine-code CALL into {@code step}
     * even though it declares an exception table, so {@code step}'s own
     * {@code catch} never ran and its Boom surfaced at the OSR return — where
     * the safe reject then re-ran every iteration since OSR entry.
     */
    public static int caughtMismatches() {
        reset();
        boolean escaped = false;
        try {
            runCaught(N);
        } catch (Boom b) {
            escaped = true;
        }
        // Nothing may escape here: `step` catches everything `maybeThrow` throws.
        int bad = wrong + (escaped ? 1 : 0);
        if (iters != N) {
            bad++;
        }
        for (int i = 0; i < N; i++) {
            if (visits[i] != 1) {
                bad++;
            }
        }
        return bad;
    }

    // ---------------------------------------------------------------
    // Shape 2 — the exception escapes the OSR'd method entirely.
    // ---------------------------------------------------------------

    static int leaf(int i) {
        if (i == TRIP) {
            throw new Boom("escape");
        }
        return i & 7;
    }

    static int runEscape(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            iters++;
            visits[i]++;
            c = c * 31 + leaf(i);
        }
        return c;
    }

    /**
     * The pure form of the defect: no handler anywhere below the caller, so the
     * OSR bail cannot pretend the loop should continue. The safe reject resumed
     * the loop from the stale pre-OSR pc anyway — 42,730 iterations executed
     * where 12,346 were asked for.
     */
    public static int escapeMismatches() {
        reset();
        boolean caught = false;
        try {
            runEscape(N);
        } catch (Boom b) {
            caught = true;
        }
        return tally(TRIP, caught);
    }

    // ---------------------------------------------------------------
    // Shape 3 — an implicit NPE raised by the OSR'd body itself.
    // ---------------------------------------------------------------

    static Cell[] cells;

    static int runNpe(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            iters++;
            visits[i]++;
            c = c * 31 + cells[i].v;
        }
        return c;
    }

    /** Sibling of {@link #escapeMismatches} for try_osr's pending-NPE drain. */
    public static int npeMismatches() {
        reset();
        cells = new Cell[N];
        for (int i = 0; i < N; i++) {
            cells[i] = (i == TRIP) ? null : new Cell(i);
        }
        boolean caught = false;
        try {
            runNpe(N);
        } catch (NullPointerException e) {
            caught = true;
        }
        return tally(TRIP, caught);
    }

    // ---------------------------------------------------------------
    // Shape 4 — an implicit AIOOBE raised by the OSR'd body itself.
    // ---------------------------------------------------------------

    static int[] shortArray;

    static int runAioobe(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            iters++;
            visits[i]++;
            c = c * 31 + shortArray[i];
        }
        return c;
    }

    /** Sibling of {@link #escapeMismatches} for try_osr's pending-AIOOBE drain. */
    public static int aioobeMismatches() {
        reset();
        shortArray = new int[TRIP];
        boolean caught = false;
        try {
            runAioobe(N);
        } catch (ArrayIndexOutOfBoundsException e) {
            caught = true;
        }
        return tally(TRIP, caught);
    }

    public static void main(String[] a) {
        System.out.println("caught=" + caughtMismatches());
        System.out.println("escape=" + escapeMismatches());
        System.out.println("npe=" + npeMismatches());
        System.out.println("aioobe=" + aioobeMismatches());
    }
}
