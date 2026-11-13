/**
 * Does hoisting a loop-invariant field read out of a loop make it run when the
 * loop does NOT?
 *
 * `walk` reads `o.v` in a loop whose trip count is a parameter. The read is
 * loop-invariant, so LICM wants it in the pre-header — but the pre-header runs
 * on every call and the body does not. With `n <= 0` and `o == null`, a read
 * that stayed in the body never happens and the method returns 0; a read that
 * moved to the pre-header dereferences null and throws.
 *
 * So the correct answer is 0, and an NPE is a miscompile, not a bug in the Java.
 */
public class ZeroTripHoist {
    static class N { int v = 7; }

    static int walk(N o, int n) {
        int a = 0;
        for (int i = 0; i < n; i++) {
            a += o.v;
        }
        return a;
    }

    public static void main(String[] args) {
        N live = new N();
        // Warm the method hard enough to reach the invocation-count door, with
        // a non-null receiver and a real trip count.
        long warm = 0;
        for (int r = 0; r < 200000; r++) {
            warm += walk(live, 4);
        }
        // Now the compiled body, entered with a null receiver and no iterations.
        int got;
        try {
            got = walk(null, 0);
        } catch (NullPointerException e) {
            System.out.println("acc=-1 ms=0 VERDICT=SPECULATED-NPE warm=" + warm);
            return;
        }
        System.out.println("acc=" + got + " ms=0 VERDICT=" + (got == 0 ? "OK" : "WRONG") + " warm=" + warm);
    }
}
