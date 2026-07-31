/**
 * Sibling of SlotReuseCategoryProbe, for the OTHER way javac slot reuse can
 * collide with the JIT's local model: the dead HIGH HALF of a cat-2 local.
 *
 * A `long`/`double` at slot N reserves N+1, which the JVM never addresses.
 * `wide_local_high_halves` collects every such N+1 and nulls its OSR register
 * assignment, on the stated reasoning that "a wide local that is never loaded
 * or stored cannot hold a value that OSR needs to preserve". javac, however,
 * happily gives slot N+1 to a REAL variable once the cat-2's scope ends -- and
 * that variable is loaded and stored.
 *
 * `highHalfReused` builds exactly that: a `long` whose scope ends before a hot
 * loop, then an `int` counter that lands in the high-half slot. If the OSR
 * trampoline skips seeding it, the counter starts at garbage.
 *
 * The controls keep the long live across the loop (so no reuse is possible)
 * and drop the long entirely.
 */
public final class HighHalfReuseProbe {

    private static int N = 400_000;
    private static long sink;

    private static long highHalfReused(int seed) {
        {
            long scoped = seed * 1_000_000_007L;
            sink = scoped;
        }
        // `scoped` is out of scope: javac may give the loop counter its
        // high-half slot.
        long total = 0;
        for (int i = 0; i < N; i++) {
            total += (i & 63) + 1;
        }
        return total;
    }

    private static long longStaysLive(int seed) {
        long scoped = seed * 1_000_000_007L;
        long total = 0;
        for (int i = 0; i < N; i++) {
            total += (i & 63) + 1;
        }
        sink = scoped;
        return total + (scoped & 0);
    }

    private static long noLong(int seed) {
        long total = seed & 0;
        for (int i = 0; i < N; i++) {
            total += (i & 63) + 1;
        }
        return total;
    }

    /** Two cat-2 scopes then two counters -- more slots to collide on. */
    private static long twoScopes(int seed) {
        {
            double d = seed * 1.5;
            sink = (long) d;
        }
        {
            long l = seed * 3L;
            sink += l;
        }
        long total = 0;
        for (int i = 0; i < N; i++) {
            for (int k = 0; k < 2; k++) {
                total += (i & 63) + k;
            }
        }
        return total;
    }

    public static void main(String[] args) {
        if (args.length > 0) {
            N = Integer.parseInt(args[0]);
        }
        for (int r = 0; r < 3; r++) {
            System.out.println("r" + r
                    + " highHalfReused=" + highHalfReused(7)
                    + " longStaysLive=" + longStaysLive(7)
                    + " noLong=" + noLong(7)
                    + " twoScopes=" + twoScopes(7));
        }
    }
}
