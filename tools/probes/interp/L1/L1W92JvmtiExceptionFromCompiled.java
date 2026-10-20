// Interpreter round i4 wave 92, lane L1: JVMTI `Exception` events from
// compiled code (page
// docs/internal/fixed-bugs/interpreter-i10-L1-proposal-exception-and-watch-events-from-compiled-code-DONE-20261224.md,
// Wave 92; switch CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED, default OFF).
//
// The shim's `Agent_OnLoad` acquires can_generate_exception_events and
// can_get_line_numbers and sets an `Exception` callback. `arm(true)` enables
// the event for the calling thread; the callback keeps, for each exception
// thrown by a method of this class, one row per distinct (throw method,
// throw line, catch method, catch line) with the exception's class and a
// count; `rows()` answers them in first-seen order. Locations are printed as
// SOURCE LINES (`GetLineNumberTable`), and every `catch` clause sits on one
// line, so the expected block below follows from this file alone.
//
// Every scenario first runs 20000 times UNARMED (so both VMs compile it),
// then 1000 times armed. Whichever tier runs a method, the event must name
// the method and line that threw and the handler that catches -- HotSpot's
// answer for compiled and interpreted code alike.
//
// Needs the native shim tools/probes/interp/L1/L1W92JvmtiExceptionFromCompiled.c,
// loaded both as an agent and with `System.load` (for `arm` / `rows`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w92ex.so tools/probes/interp/L1/L1W92JvmtiExceptionFromCompiled.c
//   (Windows: cl /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32"
//       L1W92JvmtiExceptionFromCompiled.c /Fel1w92ex.dll)
//   javac -d /tmp/l1w92ex tools/probes/interp/L1/L1W92JvmtiExceptionFromCompiled.java
//   java     -agentpath:/tmp/libl1w92ex.so -cp /tmp/l1w92ex L1W92JvmtiExceptionFromCompiled /tmp/libl1w92ex.so
//   cratonvm -agentpath:/tmp/libl1w92ex.so -cp /tmp/l1w92ex L1W92JvmtiExceptionFromCompiled /tmp/libl1w92ex.so
//
// CratonVM in both modes, with and without CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED=1
// (both must print HotSpot's block; with the switch off the VM runs
// interpreted while armed, with it on it keeps compiled code).
//
// Extra argument `implicit`: adds the stage-2 row (an
// ArrayIndexOutOfBoundsException raised by a compiled CALLEE's `iastore` and
// caught by its caller). Expected on HotSpot: the last row below; CratonVM
// with the switch on may report it at the caller's call line until stage 2
// (page, Wave 92, "Open").
//
// Extra argument `bench`: the throughput rows instead (see the end of this
// header).
//
// HOTSPOT_EXPECTED (derived from this file: the throw and catch lines are
// the lines of the `throw` statements, of the array store and of the
// one-line `catch` clauses; javac attributes a handler's first instruction
// to its `catch` line and an `athrow` / `iastore` to its statement's line):
//
//   onload: caps err=0 callbacks err=0
//   exception Ljava/lang/IllegalStateException; at sameMethod:87 catch sameMethod:88 x1000
//   exception Ljava/lang/UnsupportedOperationException; at thrower:78 catch callerCatches:94 x1000
//   exception Ljava/lang/UnsupportedOperationException; at thrower:78 catch outerCatches:100 x1000
//   exception Ljava/lang/UnsupportedOperationException; at thrower:78 catch rethrower:106 x1000
//   exception Ljava/lang/UnsupportedOperationException; at rethrower:106 catch outerOfRethrow:112 x1000
//   exception Ljava/lang/ArrayIndexOutOfBoundsException; at implicitSame:117 catch implicitSame:118 x1000
//   enable: err=0
//   disable: err=0
//   n=6000
//
// With `implicit`, right after the implicitSame row:
//
//   exception Ljava/lang/ArrayIndexOutOfBoundsException; at store:122 catch implicitCallee:128 x1000
//
// and `n=7000`.
//
// `bench` (no expected block: timings): `bench compute unarmed=<ns> armed=<ns>`
// and `bench throwcatch unarmed=<ns> armed=<ns>`, median ns per operation of
// five reps. The A/B is CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED unset vs =1:
// `armed` of `compute` should fall to about `unarmed` with the switch on.
public class L1W92JvmtiExceptionFromCompiled {
    static native int arm(boolean on);

    static native String rows();

    static int n;

    static void thrower(int i) {
        throw new UnsupportedOperationException("callee");
    }

    static void middle(int i) {
        thrower(i);
    }

    static void sameMethod() {
        try {
            throw new IllegalStateException("same");
        } catch (IllegalStateException e) { n++; }
    }

    static void callerCatches(int i) {
        try {
            thrower(i);
        } catch (UnsupportedOperationException e) { n++; }
    }

    static void outerCatches(int i) {
        try {
            middle(i);
        } catch (UnsupportedOperationException e) { n++; }
    }

    static void rethrower(int i) {
        try {
            thrower(i);
        } catch (UnsupportedOperationException e) { n++; throw e; }
    }

    static void outerOfRethrow(int i) {
        try {
            rethrower(i);
        } catch (RuntimeException e) { n++; }
    }

    static void implicitSame(int[] a, int i) {
        try {
            a[i] = 1;
        } catch (ArrayIndexOutOfBoundsException e) { n++; }
    }

    static void store(int[] a, int i) {
        a[i] = 1;
    }

    static void implicitCallee(int[] a, int i) {
        try {
            store(a, i);
        } catch (ArrayIndexOutOfBoundsException e) { n++; }
    }

    static void scenarios(int iterations, boolean implicit) {
        int[] one = new int[1];
        for (int i = 0; i < iterations; i++) {
            sameMethod();
            callerCatches(i);
            outerCatches(i);
            outerOfRethrow(i);
            implicitSame(one, i + 1);
            if (implicit) {
                implicitCallee(one, i + 1);
            }
        }
    }

    static int mix(int x) {
        x ^= x >>> 16;
        x *= 0x45d9f3b;
        return x ^ (x >>> 16);
    }

    static long compute(int iterations) {
        long h = 1;
        for (int i = 0; i < iterations; i++) {
            h = h * 31 + mix(i);
        }
        return h;
    }

    static long sink;

    /** Median ns per operation of five reps of `row` over `iterations`. */
    static double time(String row, int iterations) {
        double[] reps = new double[5];
        for (int r = 0; r < reps.length; r++) {
            long t0 = System.nanoTime();
            if (row.equals("compute")) {
                sink += compute(iterations);
            } else {
                for (int i = 0; i < iterations; i++) {
                    callerCatches(i);
                }
            }
            reps[r] = (System.nanoTime() - t0) / (double) iterations;
        }
        java.util.Arrays.sort(reps);
        return reps[reps.length / 2];
    }

    static void bench() {
        String[] rows = {"compute", "throwcatch"};
        int[] iterations = {20_000_000, 200_000};
        double[] unarmed = new double[rows.length];
        for (int k = 0; k < rows.length; k++) {
            time(rows[k], iterations[k]);
            unarmed[k] = time(rows[k], iterations[k]);
        }
        arm(true);
        double[] armed = new double[rows.length];
        for (int k = 0; k < rows.length; k++) {
            armed[k] = time(rows[k], iterations[k]);
        }
        arm(false);
        for (int k = 0; k < rows.length; k++) {
            System.out.printf("bench %s unarmed=%.1f armed=%.1f%n", rows[k], unarmed[k], armed[k]);
        }
        System.out.println("sink=" + (sink != 0));
    }

    public static void main(String[] args) {
        if (args.length < 1) {
            System.out.println(
                    "usage: L1W92JvmtiExceptionFromCompiled <path of the shim library> [implicit|bench]");
            return;
        }
        System.load(args[0]);
        String mode = args.length > 1 ? args[1] : "";
        if (mode.equals("bench")) {
            bench();
            return;
        }
        boolean implicit = mode.equals("implicit");
        scenarios(20_000, implicit);
        n = 0;
        int enable = arm(true);
        scenarios(1_000, implicit);
        int disable = arm(false);
        System.out.print(rows());
        System.out.println("enable: err=" + enable);
        System.out.println("disable: err=" + disable);
        System.out.println("n=" + n);
    }
}
