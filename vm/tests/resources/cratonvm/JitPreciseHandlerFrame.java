package cratonvm;

/**
 * Regression fixture for the JIT's exception-routing back into a compiled
 * method's OWN handler table — the RBC.6 family.
 *
 * Every entry point returns a MISMATCH COUNT (0 == pass) rather than a
 * checksum, so a duplicated loop iteration (an OSR bail re-running part of the
 * loop) re-checks instead of corrupting the expected value; only a genuine
 * routing defect can make these non-zero.
 *
 * Three shapes, each of which was silently wrong before the 2026-07-28 fixes:
 *
 * <ul>
 *   <li>{@code plainStep} — the handler reads only parameters, so this shape
 *       needs none of the precise-handler machinery and has always been
 *       compilable. Its callee {@code maybeThrow} athrows at ITS bci 13, and
 *       that bci was handed to {@code plainStep}'s drain as if it were its own;
 *       {@code plainStep}'s protected range is [0,4), so no handler matched and
 *       the exception escaped its own {@code catch}.</li>
 *   <li>{@code buildStep} — the protected range ends immediately after its last
 *       invoke, which is what javac emits for {@code try { f(x); } catch}. The
 *       precise exceptional frame used to be keyed on the invoke's SUCCESSOR,
 *       which is `end_pc` — outside the very handler that had to run.
 *       (`net.minidev.json.JSONValue.toJSONString` is the real-world instance:
 *       range [8,14), invoke at pc 11.) `sb` is also a non-parameter local read
 *       inside the handler, so this is the population the precise-handler-frame
 *       relaxation admits.</li>
 *   <li>{@code scopeStep} — `keep` is read ONLY on the path through the
 *       handler, so the handler-blind liveness behind register allocation
 *       considered it dead across the whole try and let it share a register
 *       with `other`, which IS live there.</li>
 * </ul>
 */
public class JitPreciseHandlerFrame {

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

    static void maybeThrow(boolean fail) {
        if (fail) {
            throw new Boom("boom");
        }
    }

    // Handler reads only parameters — compiles regardless of the
    // precise-handler-frame gate.
    static int plainStep(int i, boolean fail) {
        try {
            maybeThrow(fail);
        } catch (Boom b) {
            return i * 2;
        }
        return i;
    }

    public static int plainMismatches() {
        int bad = 0;
        for (int i = 0; i < 20000; i++) {
            int v = i % 97;
            boolean fail = (i % 3) == 0;
            int got;
            try {
                got = plainStep(v, fail);
            } catch (Boom escaped) {
                // plainStep's own catch did not run.
                bad++;
                continue;
            }
            if (got != (fail ? v * 2 : v)) {
                bad++;
            }
        }
        return bad;
    }

    static void append(StringBuilder sb, int i, boolean fail) {
        sb.append((char) ('0' + (i % 10)));
        if (fail) {
            throw new Boom("boom");
        }
        sb.append('x');
    }

    // `sb` is local 2 — assigned before the try, read INSIDE the handler and
    // again after it. The invoke is the last instruction of the protected range.
    static String buildStep(int i, boolean fail) {
        StringBuilder sb = new StringBuilder();
        try {
            append(sb, i, fail);
        } catch (Boom b) {
            sb.append('!');
        }
        return sb.toString();
    }

    public static int buildMismatches() {
        int bad = 0;
        for (int i = 0; i < 20000; i++) {
            boolean fail = (i % 3) == 0;
            String got;
            try {
                got = buildStep(i, fail);
            } catch (Boom escaped) {
                bad++;
                continue;
            }
            String want = "" + (char) ('0' + (i % 10)) + (fail ? '!' : 'x');
            if (!want.equals(got)) {
                bad++;
            }
        }
        return bad;
    }

    // `keep` is read ONLY on the path through the handler; `other` is read on
    // both paths. Handler-blind liveness makes them non-interfering.
    static int scopeStep(int i, boolean fail) {
        Cell keep = new Cell(i);
        Cell other = new Cell(i + 1);
        try {
            maybeThrow(fail);
        } catch (Boom b) {
            return keep.v * 3 + other.v;
        }
        return other.v;
    }

    public static int scopeMismatches() {
        int bad = 0;
        for (int i = 0; i < 20000; i++) {
            int v = i % 97;
            boolean fail = (i % 5) == 0;
            int got;
            try {
                got = scopeStep(v, fail);
            } catch (Boom escaped) {
                bad++;
                continue;
            }
            if (got != (fail ? v * 3 + (v + 1) : v + 1)) {
                bad++;
            }
        }
        return bad;
    }
}
