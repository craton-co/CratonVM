import io.netty.handler.codec.http.HttpStatusClass;
import static org.junit.jupiter.api.Assertions.assertEquals;

/**
 * `HttpResponseStatusTest.testHttpStatusClassValueOf`'s hot loop, decomposed by
 * REPLACING one rung at a time, all in the once-invoked (OSR-only) shape the
 * `@Test` method has. Every arm runs the same iteration count, so the difference
 * between two rows is the rung and nothing else.
 *
 *   bare      — the loop and the induction variable only
 *   getstatic — plus the `HttpStatusClass.UNKNOWN` read the assert needs
 *   valueOf   — plus `HttpStatusClass.valueOf(code)` (5 virtual `contains` calls)
 *   refcheck  — valueOf plus a plain reference compare, no JUnit
 *   full      — valueOf plus `Assertions.assertEquals`, i.e. the real body
 *
 * `full - refcheck` is what the JUnit assertion chain costs; `refcheck - bare`
 * is what `valueOf` costs. The 180 s per-class wall over 4 294 967 296
 * iterations is 42 ns/iter, so both numbers are budgets, not curiosities.
 */
public final class StatusLoopArmsProbe {
    static int sink;
    static final HttpStatusClass UNK = HttpStatusClass.UNKNOWN;

    static void bare(int n)      { int s = 0; for (int c = 600; c < 600 + n; c++) { s += c; } sink += s; }
    static void getstatic(int n) { int s = 0; for (int c = 600; c < 600 + n; c++) { if (HttpStatusClass.UNKNOWN != null) { s += c; } } sink += s; }
    static void valueOf(int n)   { int s = 0; for (int c = 600; c < 600 + n; c++) { if (HttpStatusClass.valueOf(c) != null) { s += c; } } sink += s; }
    static void refcheck(int n)  {
        int s = 0;
        for (int c = 600; c < 600 + n; c++) {
            HttpStatusClass k = HttpStatusClass.valueOf(c);
            if (HttpStatusClass.UNKNOWN != k) { throw new IllegalStateException(); }
            s += c;
        }
        sink += s;
    }
    static void full(int n) {
        int s = 0;
        for (int c = 600; c < 600 + n; c++) {
            HttpStatusClass k = HttpStatusClass.valueOf(c);
            assertEquals(HttpStatusClass.UNKNOWN, k);
            s += c;
        }
        sink += s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 10_000_000;
        run("bare", n);
        run("getstatic", n);
        run("valueOf", n);
        run("refcheck", n);
        run("full", n);
        System.out.println("sink=" + sink);
    }

    /** Each arm is a SEPARATE once-invoked method, so OSR is its only door. */
    static void run(String name, int n) {
        long t0 = System.nanoTime();
        switch (name) {
            case "bare": bare(n); break;
            case "getstatic": getstatic(n); break;
            case "valueOf": valueOf(n); break;
            case "refcheck": refcheck(n); break;
            default: full(n); break;
        }
        long t1 = System.nanoTime();
        System.out.printf("%-10s %8.2f ns/iter  => full 4294967296 = %.1f s%n",
                name, (double) (t1 - t0) / n, (double) (t1 - t0) / n * 4294967296.0 / 1e9);
    }
}
