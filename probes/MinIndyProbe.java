/**
 * The shape `IndyBridgeProbe` cannot express: a method whose ENTIRE body is one
 * `invokedynamic`.
 *
 * Every arm of `IndyBridgeProbe` is a loop, and a loop asks the compiler for
 * the hidden VM-pointer frame slot for some other reason — a field access, an
 * allocation, an ordinary invoke. A method like
 *
 *     static String concat(int i) { return "v=" + i; }
 *
 * asks for nothing else at all, so it is the only shape that shows whether the
 * bridge's own helper call establishes that slot. It did not: the call loaded
 * `[rbp-0]`, the saved frame pointer, as its `SharedVm` and every one of these
 * methods returned null once compiled, while every loop-shaped arm passed.
 *
 * One method per bridged bootstrap, each three or four bytecodes long, each
 * called often enough to be compiled. The first wrong answer is reported with
 * its iteration, so "it broke at the compile threshold" is visible rather than
 * inferred.
 */
public class MinIndyProbe {

    record Box(int v) {}

    static String concat(int i)    { return "v=" + i; }        // StringConcatFactory
    static Runnable lam(int i)     { return () -> {}; }        // LambdaMetafactory, non-capturing
    static Runnable cap(Box b)     { return b::hashCode; }     // LambdaMetafactory, bound ref
    static String recToString(Box b) { return b.toString(); }  // ObjectMethods -> L
    static int recHashCode(Box b)  { return b.hashCode(); }    // ObjectMethods -> I
    static boolean recEquals(Box a, Object b) { return a.equals(b); } // ObjectMethods -> Z

    static int kind(Object o) {                                 // SwitchBootstraps -> I
        return switch (o) {
            case Integer x -> x + 1;
            case String x -> x.length() + 2;
            case Box x -> x.v() + 3;
            default -> 4;
        };
    }

    static String bad = null;

    static void fail(String arm, int i, String got) {
        if (bad == null) {
            bad = arm + " wrong at i=" + i + " got=<" + got + ">";
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        Object[] xs = { 5, "seven", new Box(9), 1.5 };
        Box ref = new Box(3);
        long sink = 0;
        for (int i = 0; i < n; i++) {
            int m = i & 7;
            Box b = new Box(m);

            String c = concat(i);
            if (c == null || !c.equals("v=" + i)) fail("concat", i, String.valueOf(c));

            Runnable r = lam(i);
            if (r == null) fail("lambda", i, "null");
            else sink += 1;

            Runnable r2 = cap(b);
            if (r2 == null) fail("boundRef", i, "null");
            else sink += 1;

            String t = recToString(b);
            if (t == null || !t.equals("Box[v=" + m + "]")) fail("recToString", i, String.valueOf(t));

            int h = recHashCode(b);
            if (h != m) fail("recHashCode", i, Integer.toString(h));

            boolean e = recEquals(ref, b);
            if (e != (m == 3)) fail("recEquals", i, Boolean.toString(e));

            Object o = xs[i & 3];
            int k = kind(o);
            int want = switch (i & 3) { case 0 -> 6; case 1 -> 7; case 2 -> 12; default -> 4; };
            if (k != want) fail("patternSwitch", i, Integer.toString(k));

            sink += h + k + (e ? 1 : 0) + t.length() + c.length();
        }
        System.out.println(bad == null ? "ALL-OK sink=" + sink : "FAIL " + bad);
        System.out.flush();
        Runtime.getRuntime().halt(bad == null ? 0 : 1);
    }
}
