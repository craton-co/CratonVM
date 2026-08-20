package cratonvm;

/**
 * If the JIT collapses direct self-recursion into one frame, does it still
 * compute the right ANSWER for NON-tail self-recursion (where the caller has
 * work to do after the call returns)?
 */
public final class SWValue {
    private static final int ROUNDS = 60;

    public static void main(String[] args) {
        long bad = 0, first = -1, last = -1;
        for (int i = 0; i < ROUNDS; i++) {
            int s = sum(100);          // expect 5050
            int f = fact(12);          // expect 479001600
            int d = depthCount(64);    // expect 64
            if (i == 0) first = s;
            last = s;
            if (s != 5050 || f != 479001600 || d != 64) {
                bad++;
                if (bad <= 3) {
                    System.out.println("round#" + i + " WRONG sum=" + s + " fact=" + f + " depth=" + d);
                }
            }
        }
        System.out.println("firstSum=" + first + " lastSum=" + last + " wrongRounds=" + bad + "/" + ROUNDS);
        System.out.println(bad == 0 ? "VALUES_OK" : "VALUES_WRONG");
    }

    // non-tail: work happens after the recursive call returns
    static int sum(int n) { return n == 0 ? 0 : n + sum(n - 1); }
    static int fact(int n) { return n <= 1 ? 1 : n * fact(n - 1); }
    static int depthCount(int n) { return n == 0 ? 0 : 1 + depthCount(n - 1); }
}
