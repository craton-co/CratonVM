// No throw: the cost of Throwable.getMessage() on a held instance and of constructing one.
public class JtsParts {
    static final AssertionError AE = new AssertionError("abc");
    static final IllegalStateException ISE = new IllegalStateException("abcd");
    static volatile Object sink;

    static int msgAe() { return AE.getMessage().length(); }
    static int msgIse() { return ISE.getMessage().length(); }
    static int msgThrowable(Throwable t) { return t.getMessage().length(); }
    static int newAe(int i) { sink = new AssertionError("a"); return 1; }
    static int newIse(int i) { sink = new IllegalStateException("a"); return 1; }

    interface K { int run(int i); }

    static void row(String label, K k, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) acc += k.run(i);
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) acc += k.run(i);
            best = Math.min(best, (System.nanoTime() - t0) * 10 / n);
        }
        System.out.println("fact " + label + "-sum " + acc);
        System.out.println("time " + label + " " + best / 10.0 + " ns");
    }

    public static void main(String[] a) {
        row("msg-ae", i -> msgAe(), 2_000_000);
        row("msg-ise", i -> msgIse(), 2_000_000);
        row("msg-throwable-ae", i -> msgThrowable(AE), 2_000_000);
        row("new-ae", JtsParts::newAe, 200_000);
        row("new-ise", JtsParts::newIse, 200_000);
    }
}
