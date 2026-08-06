/**
 * Did intercepting `String.checkIndex` slow `charAt` down?
 *
 * `charAt` is one of the hottest methods in any JVM workload, and the
 * out-of-bounds fix put a registered native on its path. The reasoning says
 * that should be neutral or better -- BEFORE the fix, `String.checkIndex` ran
 * as bytecode and called the `Preconditions.checkIndex` NATIVE one frame
 * deeper, so the funnel was already being paid; the fix intercepts one frame
 * earlier and pays it once instead. But "should be" is not a measurement, and
 * this feature has already had three performance claims fail to survive one.
 *
 * Two shapes, because the JIT changes the answer:
 *   hot   -- a tight loop the JIT will compile, where `charAt` may be
 *            intrinsified and neither implementation is called at all
 *   cold  -- many distinct short strings, closer to parsing/formatting code,
 *            where the interpreter actually reaches the bounds check
 *
 * Checksums must match between arms or the timings compare different work.
 * Run A-B-B-A interleaved against the pre-fix binary.
 */
public class StringCharAtCostProbe {

    static final int ITERS = 300000;

    static long hot(int iters) {
        String s = "the quick brown fox jumps over the lazy dog";
        long sum = 0;
        int n = s.length();
        for (int i = 0; i < iters; i++) {
            for (int j = 0; j < n; j++) {
                sum += s.charAt(j);
            }
        }
        return sum;
    }

    static long cold(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            String s = "row-" + i;
            for (int j = 0; j < s.length(); j++) {
                sum += s.charAt(j);
            }
        }
        return sum;
    }

    /// The throwing path itself: building the message is new work on a path
    /// that previously produced a null message, so it deserves its own number.
    static long throwing(int iters) {
        String s = "hello world!";
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            try {
                s.charAt(-1);
            } catch (IndexOutOfBoundsException e) {
                // Deliberately the SUPERCLASS. Catching
                // StringIndexOutOfBoundsException here made this probe unable to
                // run against the pre-fix binary at all: `charAt(-1)` threw
                // ArrayIndexOutOfBoundsException there, the catch missed it, and
                // the whole run died with no output. That is the bug being
                // measured, so the harness must not depend on it being fixed.
                sum += e.getMessage() == null ? 1 : e.getMessage().length();
            }
        }
        return sum;
    }

    static void run(String name, int iters) {
        long t0 = System.currentTimeMillis();
        long sum;
        switch (name) {
            case "hot":  sum = hot(iters);  break;
            case "cold": sum = cold(iters); break;
            default:     sum = throwing(iters / 10); break;
        }
        System.out.println(name + " ms=" + (System.currentTimeMillis() - t0) + " checksum=" + sum);
    }

    public static void main(String[] args) {
        for (String w : new String[] { "hot", "cold", "throwing" }) {
            run(w + "-warmup", ITERS / 20);
        }
        for (int round = 0; round < 3; round++) {
            System.out.println("--- round " + round);
            for (String w : new String[] { "hot", "cold", "throwing" }) {
                run(w, ITERS);
            }
        }
        System.out.println("CHARAT-COST-PROBE-DONE");
    }
}
