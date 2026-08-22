import java.util.function.*;

/**
 * Prices `invokedynamic` lambda INSTANTIATION against the equivalent anonymous
 * class, plus the call itself.
 *
 * JVMS 5.4.3.6: an `invokedynamic` instruction is linked ONCE — the bootstrap
 * method runs on first execution and the resulting CallSite is bound to that
 * instruction permanently. Every later execution is supposed to be an ordinary
 * indy call into the already-linked target, which for LambdaMetafactory is
 * "allocate the capture object" (non-capturing lambdas do not even allocate).
 * If a VM re-bootstraps per execution, THAT is what this measures: the gap
 * between the `lambda*` arms and the `anon*` arms below.
 *
 * Each loop is in its own method so invocation-count tier-up applies.
 */
public class IndyProbe {
    static Object sink;
    static long lsink;
    static int captured = 7;

    // --- instantiation only, never called ---------------------------------
    static void lambdaNonCapturing(int n) {
        for (int i = 0; i < n; i++) sink = (IntUnaryOperator) (x -> x + 1);
    }
    static void lambdaCapturingLocal(int n) {
        for (int i = 0; i < n; i++) { int c = i; sink = (IntUnaryOperator) (x -> x + c); }
    }
    static void lambdaCapturingStatic(int n) {
        for (int i = 0; i < n; i++) sink = (IntUnaryOperator) (x -> x + captured);
    }
    static void methodRef(int n) {
        for (int i = 0; i < n; i++) sink = (IntUnaryOperator) IndyProbe::plusOne;
    }
    static int plusOne(int x) { return x + 1; }

    static void anonNonCapturing(int n) {
        for (int i = 0; i < n; i++) sink = new IntUnaryOperator() {
            public int applyAsInt(int x) { return x + 1; } };
    }
    static void anonCapturingLocal(int n) {
        for (int i = 0; i < n; i++) { final int c = i; sink = new IntUnaryOperator() {
            public int applyAsInt(int x) { return x + c; } }; }
    }

    // --- instantiate AND call ---------------------------------------------
    static void lambdaCall(int n) {
        for (int i = 0; i < n; i++) { IntUnaryOperator f = x -> x + 1; lsink += f.applyAsInt(i); }
    }
    static void anonCall(int n) {
        for (int i = 0; i < n; i++) {
            IntUnaryOperator f = new IntUnaryOperator() {
                public int applyAsInt(int x) { return x + 1; } };
            lsink += f.applyAsInt(i);
        }
    }
    // --- call a HOISTED lambda (instantiated once, called n times) ---------
    static void hoistedLambdaCall(int n) {
        IntUnaryOperator f = x -> x + 1;
        for (int i = 0; i < n; i++) lsink += f.applyAsInt(i);
    }
    static void hoistedAnonCall(int n) {
        IntUnaryOperator f = new IntUnaryOperator() {
            public int applyAsInt(int x) { return x + 1; } };
        for (int i = 0; i < n; i++) lsink += f.applyAsInt(i);
    }
    // --- string concat is also invokedynamic (makeConcatWithConstants) -----
    static void stringConcatIndy(int n) {
        for (int i = 0; i < n; i++) sink = "a" + i + "b";
    }
    static void control(int n) { for (int i = 0; i < n; i++) lsink += i; }

    interface Arm { void run(int n); }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 2;
        String[] names = {"control", "lambda non-capturing", "lambda capture local",
            "lambda capture static", "method ref", "anon non-capturing", "anon capture local",
            "lambda new+call", "anon new+call", "hoisted lambda call", "hoisted anon call",
            "string concat (indy)"};
        Arm[] arms = {IndyProbe::control, IndyProbe::lambdaNonCapturing,
            IndyProbe::lambdaCapturingLocal, IndyProbe::lambdaCapturingStatic,
            IndyProbe::methodRef, IndyProbe::anonNonCapturing, IndyProbe::anonCapturingLocal,
            IndyProbe::lambdaCall, IndyProbe::anonCall, IndyProbe::hoistedLambdaCall,
            IndyProbe::hoistedAnonCall, IndyProbe::stringConcatIndy};
        for (int pass = 0; pass < passes; pass++) {
            System.out.println("--- pass " + pass + " ---");
            for (int k = 0; k < names.length; k++) {
                arms[k].run(Math.min(n, 20000));
                long t0 = System.nanoTime();
                arms[k].run(n);
                long d = System.nanoTime() - t0;
                System.out.printf("%-24s %9.1f ns/op%n", names[k], (double) d / n);
                System.out.flush();
            }
        }
        System.out.println("PROBE-DONE " + lsink + " " + (sink != null));
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
