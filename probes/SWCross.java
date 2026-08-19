package cratonvm;

/** Does a CROSS-METHOD callee below a hot self-recursion keep its frame? */
public final class SWCross {
    private static final int ROUNDS = 40;

    public static void main(String[] args) {
        for (int i = 0; i < ROUNDS; i++) {
            StackTraceElement[] t = grab(64);
            boolean hasHelper = false, hasRecurse = false;
            for (StackTraceElement e : t) {
                if (e.getMethodName().equals("helper")) hasHelper = true;
                if (e.getMethodName().equals("recurse")) hasRecurse = true;
            }
            if (i == 0 || i == ROUNDS - 1) {
                System.out.println("round#" + i + " n=" + t.length
                        + " helper=" + hasHelper + " recurse=" + hasRecurse
                        + " top=" + t[0].getMethodName());
            }
        }
        System.out.println("DONE");
    }

    static StackTraceElement[] grab(int d) {
        try { recurse(d); return null; }
        catch (IllegalStateException e) { return e.getStackTrace(); }
    }
    static void recurse(int d) { if (d == 0) { helper(); return; } recurse(d - 1); }
    static void helper() { throw new IllegalStateException("h"); }
}
