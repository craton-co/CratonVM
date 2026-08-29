/** Prices the `StringBuilder` write path — the A/B that `l2-strings-residuals`
 *  N1 asks for.
 *
 *  The control is `f52fa3fa6`, which has the ten null-contract fixes and the 62
 *  `StringBuffer` retirements but NOT the layout migration, so a run of this
 *  against both binaries isolates the migration and nothing else.
 *
 *  METHOD, each item learned by getting it wrong on this host:
 *
 *    * every timed loop is in its OWN static method, never in `main` — a
 *      benchmark loop in `main` measures interpreted code, because `main` is
 *      entered once and never becomes hot;
 *    * a warm-up pass runs before the timed one, and its result is discarded;
 *    * the result of every loop is accumulated into a sink that is printed, so
 *      nothing can be optimised away;
 *    * only ns/op is printed, never a wall clock or a thread name, so the
 *      output diffs cleanly;
 *    * the caller interleaves ABBA across binaries. One A/B pair on this host
 *      is not a result.
 */
public class SbLayoutBench {

    static long sink = 0;

    // ---- the five shapes -------------------------------------------------

    /** `append(String)` into a builder that grows past several doublings —
     *  the shape almost every real caller has. */
    static long appendString(int iters, int per) {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            StringBuilder b = new StringBuilder();
            for (int j = 0; j < per; j++) {
                b.append("abcdefgh");
            }
            sink += b.length();
        }
        return System.nanoTime() - t0;
    }

    /** `append(char)`, the narrowest possible write. */
    static long appendChar(int iters, int per) {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            StringBuilder b = new StringBuilder();
            for (int j = 0; j < per; j++) {
                b.append('x');
            }
            sink += b.length();
        }
        return System.nanoTime() - t0;
    }

    /** `append(int)` — the shape javac emits for string concatenation of a
     *  number, and the one the interpreter has an intrinsic door for. */
    static long appendInt(int iters, int per) {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            StringBuilder b = new StringBuilder();
            for (int j = 0; j < per; j++) {
                b.append(j);
            }
            sink += b.length();
        }
        return System.nanoTime() - t0;
    }

    /** `toString()` of an already-built builder: the whole-payload read. */
    static long toStringOf(int iters, int per) {
        StringBuilder b = new StringBuilder();
        for (int j = 0; j < per; j++) {
            b.append("abcdefgh");
        }
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            sink += b.toString().length();
        }
        return System.nanoTime() - t0;
    }

    /** `charAt` in a loop: the single-character read, which the migration
     *  routes through a layout resolution it did not have before. */
    static long charAtLoop(int iters, int per) {
        StringBuilder b = new StringBuilder();
        for (int j = 0; j < per; j++) {
            b.append("abcdefgh");
        }
        int n = b.length();
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            long s = 0;
            for (int j = 0; j < n; j++) {
                s += b.charAt(j);
            }
            sink += s;
        }
        return System.nanoTime() - t0;
    }

    /** A builder that INFLATES: latin1 for a while, then a non-latin1
     *  character, then more appends. The one shape whose cost the migration
     *  genuinely changes rather than merely re-routes. */
    static long inflating(int iters, int per) {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            StringBuilder b = new StringBuilder();
            for (int j = 0; j < per; j++) {
                b.append("abcdefgh");
            }
            b.append('\u20ac');
            for (int j = 0; j < per; j++) {
                b.append("abcdefgh");
            }
            sink += b.length();
        }
        return System.nanoTime() - t0;
    }

    // ---- harness ---------------------------------------------------------

    interface Shape {
        long run(int iters, int per);
    }

    static void bench(String name, Shape s, int warmIters, int iters, int per) {
        s.run(warmIters, per);          // warm-up, discarded
        long ns = s.run(iters, per);
        long ops = (long) iters * per;
        System.out.println(name + " ns/op " + (ns / ops));
    }

    public static void main(String[] args) {
        int scale = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        bench("appendString", SbLayoutBench::appendString, 200, 2000 * scale, 400);
        bench("appendChar", SbLayoutBench::appendChar, 200, 2000 * scale, 400);
        bench("appendInt", SbLayoutBench::appendInt, 200, 2000 * scale, 400);
        bench("toString", SbLayoutBench::toStringOf, 200, 20000 * scale, 400);
        bench("charAt", SbLayoutBench::charAtLoop, 50, 500 * scale, 400);
        bench("inflating", SbLayoutBench::inflating, 100, 1000 * scale, 400);
        System.out.println("sink " + (sink != 0));
        System.out.println("DONE SbLayoutBench");
    }
}
