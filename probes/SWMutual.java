package cratonvm;

/**
 * Discriminator: are JIT frames enumerated PER ACTIVATION or PER DISTINCT METHOD?
 * Mutual recursion a->b->a->b... 64 deep uses 2 distinct methods and has no two
 * CONSECUTIVE identical frames.
 *   per-activation   -> ~67
 *   per-distinct-method -> ~4
 *   consecutive-dedup   -> ~67 (nothing is consecutive-identical)
 */
public final class SWMutual {
    private static final int ROUNDS = 40;

    public static void main(String[] args) {
        for (int i = 0; i < ROUNDS; i++) {
            int n = depth(64);
            if (i == 0 || i == ROUNDS - 1) {
                System.out.println("round#" + i + " mutualFrames=" + n);
            }
        }
        System.out.println("DONE");
    }

    static int depth(int d) {
        try { a(d); return -1; } catch (IllegalStateException e) { return e.getStackTrace().length; }
    }
    static void a(int d) { if (d == 0) throw new IllegalStateException("a"); b(d - 1); }
    static void b(int d) { if (d == 0) throw new IllegalStateException("b"); a(d - 1); }
}
