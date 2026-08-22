import java.util.function.IntUnaryOperator;

/**
 * Single arm: call ONE hoisted, non-capturing lambda n times.
 *
 * The byte-identical anonymous-class twin runs at 36 ns/op on the same VM
 * (`IndyProbe`), so everything above that is lambda-proxy dispatch and nothing
 * else. Use arg 2 = "anon" to run the twin instead, as the control.
 */
public class HoistedLambdaProbe {
    static long sink;

    static void lambdaLoop(int n) {
        IntUnaryOperator f = x -> x + 1;
        for (int i = 0; i < n; i++) sink += f.applyAsInt(i);
    }

    static void anonLoop(int n) {
        IntUnaryOperator f = new IntUnaryOperator() {
            public int applyAsInt(int x) { return x + 1; }
        };
        for (int i = 0; i < n; i++) sink += f.applyAsInt(i);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 500000;
        boolean anon = args.length > 1 && "anon".equals(args[1]);
        if (anon) anonLoop(20000); else lambdaLoop(20000);
        long t0 = System.nanoTime();
        if (anon) anonLoop(n); else lambdaLoop(n);
        long d = System.nanoTime() - t0;
        System.out.printf("%s %8.1f ns/op  sink=%d%n", anon ? "anon   " : "lambda ",
                (double) d / n, sink);
        System.out.flush();
    }
}
