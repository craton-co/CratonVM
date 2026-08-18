import io.netty.handler.codec.http.HttpStatusClass;
import org.junit.jupiter.api.Assertions;

/**
 * What does `assertEquals(UNKNOWN, k)` cost, rung by rung?
 *
 * `StatusLoopArmsProbe` measured 2026-08-17 on the Azure host, CratonVM:
 * `valueOf` 17.71 ns/iter and `full` 80.57 — so the JUnit assertion chain is
 * **63 ns of the 80**, not the "four more frames" beside a ~30 ns `valueOf`
 * that `httpresponsestatustest-exhaustive-loop-timeout-20260816.md` estimated.
 * HotSpot's own chain is ~11.7 ns on the same probe. That makes the chain, not
 * `valueOf`, the thing standing between this class and its 42 ns/iteration
 * budget, and this probe is where it gets split.
 *
 * The rungs, each adding exactly one level to the one above it. `bare` and
 * `valueOfOnly` are the controls — every row is a delta against the row above,
 * and the deltas are the only numbers worth reading:
 *
 *   bare        — the loop and the induction variable
 *   valueOfOnly — plus `HttpStatusClass.valueOf(code)`
 *   refCompare  — plus `k != UNKNOWN`, a reference compare, no call
 *   enumEquals  — plus `UNKNOWN.equals(k)`, ONE call: `Enum.equals` is
 *                 `this == other`, so anything above a plain virtual call here
 *                 is dispatch overhead rather than work
 *   objectsEq   — plus `AssertionUtils.objectsAreEqual`, reached the way JUnit
 *                 reaches it. Package-private, so this rung goes through
 *                 `Assertions.assertEquals`'s two-argument overload minus the
 *                 message overload — see `assertTwoArg` below
 *   assertFull  — the real `Assertions.assertEquals(Object, Object)`
 *
 * Every arm is a SEPARATE once-invoked method, because that is the shape a
 * `@Test` body has and OSR is then its only door out of the interpreter. One
 * method called six times would be method-entry compiled after the first, which
 * is a different tier answering a different question.
 *
 *   javac -nowarn -cp "<netty+junit cp>:." -d . probes/AssertChainProbe.java
 *   java                       -cp "<cp>:." AssertChainProbe 20000000
 *   cratonvm --java-home <jdk> -cp "<cp>:." AssertChainProbe 20000000
 */
public final class AssertChainProbe {

    static int sink;
    static final HttpStatusClass UNK = HttpStatusClass.UNKNOWN;

    static void bare(int n) {
        int s = 0;
        for (int c = 600; c < 600 + n; c++) { s += c; }
        sink += s;
    }

    static void valueOfOnly(int n) {
        int s = 0;
        for (int c = 600; c < 600 + n; c++) {
            HttpStatusClass k = HttpStatusClass.valueOf(c);
            if (k == null) { s++; }
            s += c;
        }
        sink += s;
    }

    static void refCompare(int n) {
        int s = 0;
        for (int c = 600; c < 600 + n; c++) {
            HttpStatusClass k = HttpStatusClass.valueOf(c);
            if (UNK != k) { s++; }
            s += c;
        }
        sink += s;
    }

    static void enumEquals(int n) {
        int s = 0;
        for (int c = 600; c < 600 + n; c++) {
            HttpStatusClass k = HttpStatusClass.valueOf(c);
            if (!UNK.equals(k)) { s++; }
            s += c;
        }
        sink += s;
    }

    /**
     * `AssertionUtils.objectsAreEqual` is package-private to
     * `org.junit.jupiter.api`, so it cannot be called from here. `assertTwoArg`
     * is the closest reachable rung below the public entry point: it reproduces
     * what `AssertEquals.assertEquals(Object,Object)` does — the null-aware
     * `equals` that `objectsAreEqual` is — without the message-supplier
     * overload above it.
     */
    static boolean assertTwoArg(Object expected, Object actual) {
        if (expected == null) {
            return actual == null;
        }
        return expected.equals(actual);
    }

    static void objectsEq(int n) {
        int s = 0;
        for (int c = 600; c < 600 + n; c++) {
            HttpStatusClass k = HttpStatusClass.valueOf(c);
            if (!assertTwoArg(UNK, k)) { s++; }
            s += c;
        }
        sink += s;
    }

    static void assertFull(int n) {
        int s = 0;
        for (int c = 600; c < 600 + n; c++) {
            HttpStatusClass k = HttpStatusClass.valueOf(c);
            Assertions.assertEquals(HttpStatusClass.UNKNOWN, k);
            s += c;
        }
        sink += s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000_000;
        double prev = 0;
        prev = run("bare", n, time(() -> bare(n)), prev, false);
        prev = run("valueOfOnly", n, time(() -> valueOfOnly(n)), prev, true);
        prev = run("refCompare", n, time(() -> refCompare(n)), prev, true);
        prev = run("enumEquals", n, time(() -> enumEquals(n)), prev, true);
        prev = run("objectsEq", n, time(() -> objectsEq(n)), prev, true);
        run("assertFull", n, time(() -> assertFull(n)), prev, true);
        System.out.println("sink=" + sink);
    }

    static long time(Runnable r) {
        long a = System.nanoTime();
        r.run();
        return System.nanoTime() - a;
    }

    static double run(String name, int n, long ns, double prev, boolean delta) {
        double per = (double) ns / n;
        if (delta) {
            System.out.printf("%-12s %8.2f ns/iter   (+%6.2f over the rung above)   full 4294967296 = %7.1f s%n",
                    name, per, per - prev, per * 4294967296.0 / 1e9);
        } else {
            System.out.printf("%-12s %8.2f ns/iter   (control)                      full 4294967296 = %7.1f s%n",
                    name, per, per * 4294967296.0 / 1e9);
        }
        return per;
    }
}
