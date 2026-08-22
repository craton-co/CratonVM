import java.util.function.IntUnaryOperator;

/**
 * Scopes the `osr-DENY (unbridged invokedynamic)` rule.
 *
 * A method containing an unbridged `invokedynamic` is permanently denied OSR
 * (jit_bridge.rs, the RBC.7 guard). This probe separates the two ways a method
 * can reach the JIT so the blast radius is measurable:
 *
 *   osrShape       — one long-running call; only OSR can compile it.
 *   invokeShape    — a small method containing the indy, called n times; only
 *                    the invocation counter can compile it.
 *   controlOsr     — osrShape with the lambda hoisted OUT, so the loop method
 *                    itself has no indy. The OSR control.
 *
 * If invokeShape is fast and osrShape is slow, the deny is OSR-only.
 */
public class IndyScopeProbe {
    static long sink;
    static IntUnaryOperator hoisted = x -> x + 1;

    // indy INSIDE the loop method -> that method needs OSR and is denied
    static void osrShape(int n) {
        IntUnaryOperator f = x -> x + 1;
        for (int i = 0; i < n; i++) sink += f.applyAsInt(i);
    }

    // indy inside a SMALL method called n times -> invocation-count tier-up
    static int makeAndCall(int i) {
        IntUnaryOperator f = x -> x + 1;
        return f.applyAsInt(i);
    }
    static void invokeShape(int n) {
        for (int i = 0; i < n; i++) sink += makeAndCall(i);
    }

    // the SAM call in a small method called n times, lambda made once
    static int callOnly(int i) { return hoisted.applyAsInt(i); }
    static void callShape(int n) {
        for (int i = 0; i < n; i++) sink += callOnly(i);
    }

    // control: loop method has NO indy at all (lambda is a static field)
    static void controlOsr(int n) {
        IntUnaryOperator f = hoisted;
        for (int i = 0; i < n; i++) sink += f.applyAsInt(i);
    }

    interface Arm { void run(int n); }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 1000000;
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 2;
        String[] names = {"osrShape (indy in loop)", "controlOsr (indy hoisted out)",
                          "invokeShape (indy per call)", "callShape (call per call)"};
        Arm[] arms = {IndyScopeProbe::osrShape, IndyScopeProbe::controlOsr,
                      IndyScopeProbe::invokeShape, IndyScopeProbe::callShape};
        for (int p = 0; p < passes; p++) {
            System.out.println("--- pass " + p + " ---");
            for (int k = 0; k < names.length; k++) {
                arms[k].run(Math.min(n, 20000));
                long t0 = System.nanoTime();
                arms[k].run(n);
                long d = System.nanoTime() - t0;
                System.out.printf("%-30s %9.1f ns/op%n", names[k], (double) d / n);
                System.out.flush();
            }
        }
        System.out.println("PROBE-DONE " + sink);
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
