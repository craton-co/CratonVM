/**
 * Prices BOTH sides of the speculative `String` receiver guard CratonVM emits
 * at a `java/lang/CharSequence`-declared `length()` / `charAt(int)` call site.
 *
 * The guard's miss edge is a DEOPT, not a fall-through to dispatch, so the two
 * sides are not symmetric and neither can be read alone:
 *
 *  * `hitLen` / `hitChar` — the receiver really IS a `String`. The guard passes
 *    on every call, the inline String-layout decode runs, and the arm measures
 *    what turning the speculation off COSTS.
 *  * `missLen` / `missChar` — the receiver is a `StringBuilder`, i.e. a
 *    `CharSequence` that is not a `String`. The guard fails on every call, and
 *    the arm measures what leaving the speculation on COSTS. That is the shape
 *    `HttpHeaderValidationUtil.validateValidHeaderValue` has in netty's
 *    `HttpHeaderValidationUtilTest`.
 *
 * Each arm has its OWN call site so no site is polymorphic: a shared helper
 * would make one MIC serve both receivers and price neither.
 *
 * Run both arms of the A/B against the SAME binary:
 *
 *   cratonvm --java-home <jdk> -cp . CharSeqStringIntrinsicProbe 20000000
 *   CRATONVM_JIT_CHARSEQ_STRING_INTRINSIC=0 cratonvm ... CharSeqStringIntrinsicProbe 20000000
 *
 * See docs/known-issues/netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md.
 */
public final class CharSeqStringIntrinsicProbe {

    /** Chunk size. Each loop method is invoked many times so it crosses the
     *  method-entry threshold rather than relying on OSR. */
    private static final int CHUNK = 65536;

    private static int hitLen(CharSequence cs, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += cs.length();
        }
        return s;
    }

    private static int missLen(CharSequence cs, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += cs.length();
        }
        return s;
    }

    private static int hitChar(CharSequence cs, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += cs.charAt(i & 7);
        }
        return s;
    }

    private static int missChar(CharSequence cs, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += cs.charAt(i & 7);
        }
        return s;
    }

    private static long sink;

    private static double run(int arm, CharSequence cs, long iters) {
        long chunks = Math.max(1, iters / CHUNK);
        long t0 = System.nanoTime();
        for (long c = 0; c < chunks; c++) {
            switch (arm) {
                case 0: sink += hitLen(cs, CHUNK); break;
                case 1: sink += missLen(cs, CHUNK); break;
                case 2: sink += hitChar(cs, CHUNK); break;
                default: sink += missChar(cs, CHUNK); break;
            }
        }
        long t1 = System.nanoTime();
        return (double) (t1 - t0) / (double) (chunks * CHUNK);
    }

    public static void main(String[] args) {
        long n = args.length > 0 ? Long.parseLong(args[0]) : 20_000_000L;
        CharSequence str = "abcdefgh";
        CharSequence sb = new StringBuilder("abcdefgh");

        // Warm every arm before timing any of it, so the first timed arm does
        // not pay for the compiles the later ones inherit.
        run(0, str, CHUNK * 8L);
        run(1, sb, CHUNK * 8L);
        run(2, str, CHUNK * 8L);
        run(3, sb, CHUNK * 8L);

        double hl = run(0, str, n);
        double ml = run(1, sb, n);
        double hc = run(2, str, n);
        double mc = run(3, sb, n);

        System.out.printf("hitLen   (String     via CharSequence) %8.2f ns/iter%n", hl);
        System.out.printf("missLen  (StringBuilder via CharSequence) %8.2f ns/iter%n", ml);
        System.out.printf("hitChar  (String     via CharSequence) %8.2f ns/iter%n", hc);
        System.out.printf("missChar (StringBuilder via CharSequence) %8.2f ns/iter%n", mc);
        if (sink == Long.MIN_VALUE) {
            throw new IllegalStateException();
        }
    }
}
