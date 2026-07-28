// Discriminates WHY a JIT-compiled `finally` is skipped.
//
// route_jit_exception_through_method() honours a catch-all (catch_type == 0,
// i.e. a javac `finally`) only when the throw PC is known, OR when the
// catch-all's protected region spans the whole method. The throw PC is known
// for an explicit `athrow` in the compiled method itself (athrow_bci), and
// unknown for an exception that propagates out of a callee.
//
// So: DIRECT (athrow in this method) should balance, CALLEE (throw from a
// callee) should leak, if that rule is the cause.
public class FinallyThrowSiteProbe {

    static int n = 0;
    static long sink = 0;

    static class Boom extends RuntimeException {
        Boom() { super(null, null, false, false); }
    }

    static void thrower(int i) {
        sink += i;
        if ((i % 7) == 3) {
            throw new Boom();
        }
    }

    // Throw site is an explicit athrow inside this method's own body.
    static void direct(int i) {
        try {
            n++;
            sink += i;
            if ((i % 7) == 3) {
                throw new Boom();
            }
        } finally {
            n--;
        }
    }

    // Throw site is inside a callee; the PC is not recoverable.
    static void callee(int i) {
        try {
            n++;
            thrower(i);
        } finally {
            n--;
        }
    }

    // Callee throw, but the finally's protected region covers the whole
    // method body (nothing before the try, nothing after the handler).
    static void calleeWholeMethod(int i) {
        try {
            n++;
            thrower(i);
        } finally {
            n--;
        }
    }

    interface Shape { void run(int i); }

    static void drive(String name, Shape s, int iterations) {
        n = 0;
        int firstLeak = -1;
        for (int i = 0; i < iterations; i++) {
            try {
                s.run(i);
            } catch (Boom e) {
                // expected
            }
            if (n != 0 && firstLeak < 0) {
                firstLeak = i;
            }
        }
        System.out.println(String.format("%-18s final=%-8d firstLeak=%-8d %s",
                name, n, firstLeak, (n == 0 ? "OK" : "LEAK")));
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        drive("DIRECT-athrow", FinallyThrowSiteProbe::direct, iterations);
        drive("CALLEE-throw", FinallyThrowSiteProbe::callee, iterations);
        drive("CALLEE-whole", FinallyThrowSiteProbe::calleeWholeMethod, iterations);
        System.out.println("sink=" + sink);
    }
}
