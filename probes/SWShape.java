package cratonvm;

/**
 * Is the tier-up frame loss specific to TAIL self-recursion, or does any hot
 * compiled call chain lose frames?
 *   A = tail self-recursion        (return recurse(d-1);)
 *   B = non-tail self-recursion    (int r = f(d-1); return r + 1;)
 *   C = distinct-method chain, no recursion at all
 */
public final class SWShape {
    private static final int ROUNDS = 40;

    public static void main(String[] args) {
        for (int i = 0; i < ROUNDS; i++) {
            int a = tailDepth(64), b = nonTailDepth(64), c = chainDepth();
            if (i == 0 || i == ROUNDS - 1) {
                System.out.println("round#" + i + " tail=" + a + " nonTail=" + b + " chain=" + c);
            }
        }
        System.out.println("DONE");
    }

    static int tailDepth(int d) {
        try { tail(d); return -1; } catch (IllegalStateException e) { return e.getStackTrace().length; }
    }
    static void tail(int d) { if (d == 0) throw new IllegalStateException("t"); tail(d - 1); }

    static int nonTailDepth(int d) {
        try { return nonTail(d); } catch (IllegalStateException e) { return e.getStackTrace().length; }
    }
    static int nonTail(int d) {
        if (d == 0) throw new IllegalStateException("n");
        int r = nonTail(d - 1);
        return r + 1;
    }

    static int chainDepth() {
        try { c0(); return -1; } catch (IllegalStateException e) { return e.getStackTrace().length; }
    }
    static void c0() { c1(); }
    static void c1() { c2(); }
    static void c2() { c3(); }
    static void c3() { c4(); }
    static void c4() { c5(); }
    static void c5() { c6(); }
    static void c6() { c7(); }
    static void c7() { throw new IllegalStateException("c"); }
}
