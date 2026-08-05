import java.util.concurrent.atomic.AtomicInteger;

/**
 * What makes a NATIVE call cost ~950 ns when a Java call costs 8.4 ns?
 *
 * Established: an ordinary interpreted-then-compiled Java call is 8.4 ns here,
 * and `Thread.onSpinWait()` — which the interpreter now answers inline, without
 * entering `safe_native_call` at all — is 4.4 ns. Everything that DOES enter
 * `safe_native_call` costs two to three orders of magnitude more. This probe
 * varies the native's shape to find which part of that funnel is expensive:
 *
 *   receiver?      static native      vs  instance native
 *   object args?   primitive-only     vs  object argument
 *   returns?       void / int         vs  object
 *   intrinsic?     in the interpreter intrinsic table  vs  registry native
 *
 * If pinning and the GC-forwarding barrier dominate, the object-argument and
 * object-returning rungs separate from the primitive ones. If they do not, the
 * cost is the funnel's fixed bookkeeping (thread-state transitions, ring
 * buffer, catch_unwind, GC-pressure probes) and every rung lands together.
 *
 * Multi-pass; read the LAST column. A single pass here is warm-up, not a
 * measurement — see AqsBreakdownProbe's header for what that cost already.
 */
public final class NativeShapeProbe {

    private static final int PASSES = 4;
    private static final int ROUNDS = 2_000_000;

    private static long sink;
    private static Object osink;

    private interface Rung { long run(int n); }

    private static void pass(String label, Rung r) {
        System.out.printf("%-46s", label);
        for (int i = 0; i < PASSES; i++) {
            System.out.printf("%11.1f", r.run(ROUNDS) / (double) ROUNDS);
        }
        System.out.println();
    }

    // ---- rungs (each its own inline loop) --------------------------------

    /** No call at all. */
    private static long rControl(int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a += i; }
        sink += a; return System.nanoTime() - t0;
    }

    /** INTRINSIC, static, primitive in/out. */
    private static long rMathAbs(int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a += Math.abs(i); }
        sink += a; return System.nanoTime() - t0;
    }

    /** INTRINSIC, instance receiver, primitive out. */
    private static long rStringLength(String s, int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a += s.length(); }
        sink += a; return System.nanoTime() - t0;
    }

    /** REGISTRY native, static, no args, returns an OBJECT. */
    private static long rCurrentThread(int n) {
        long t0 = System.nanoTime(); Object o = null;
        for (int i = 0; i < n; i++) { o = Thread.currentThread(); }
        osink = o; return System.nanoTime() - t0;
    }

    /** REGISTRY native, static, no args, primitive out. */
    private static long rNanoTime(int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a += System.nanoTime(); }
        sink += a; return System.nanoTime() - t0;
    }

    /** REGISTRY native, instance receiver, no args, primitive out. */
    private static long rAtomicGet(AtomicInteger ai, int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a += ai.get(); }
        sink += a; return System.nanoTime() - t0;
    }

    /** REGISTRY native, instance receiver, primitive ARG, primitive out. */
    private static long rAtomicCas(AtomicInteger ai, int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { if (ai.compareAndSet(0, 0)) { a++; } }
        sink += a; return System.nanoTime() - t0;
    }

    /** REGISTRY native, static, OBJECT arg, primitive out. */
    private static long rIdentityHash(Object o, int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a += System.identityHashCode(o); }
        sink += a; return System.nanoTime() - t0;
    }

    /** Interpreter-answered, no funnel at all — the floor. */
    private static long rOnSpinWait(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { Thread.onSpinWait(); }
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) {
        AtomicInteger ai = new AtomicInteger();
        String s = "abcdefgh";
        Object o = new Object();

        System.out.printf("%-46s", "rung");
        for (int i = 1; i <= PASSES; i++) { System.out.printf("%11d", i); }
        System.out.println("   (ns/op per pass; read the LAST)");

        pass("control: no call", NativeShapeProbe::rControl);
        pass("interpreter-answered: Thread.onSpinWait", NativeShapeProbe::rOnSpinWait);
        System.out.println();
        pass("INTRINSIC static prim:  Math.abs(int)", NativeShapeProbe::rMathAbs);
        pass("INTRINSIC recv   prim:  String.length()", n -> rStringLength(s, n));
        System.out.println();
        pass("NATIVE static, no arg -> long: nanoTime", NativeShapeProbe::rNanoTime);
        pass("NATIVE static, no arg -> OBJ:  currentThread", NativeShapeProbe::rCurrentThread);
        pass("NATIVE static, OBJ arg -> int: identityHashCode", n -> rIdentityHash(o, n));
        pass("NATIVE recv, no arg -> int:    AtomicInteger.get", n -> rAtomicGet(ai, n));
        pass("NATIVE recv, prim args -> bool: Atomic.CAS", n -> rAtomicCas(ai, n));

        if (sink == 42 && osink == null) { System.out.println("(unreachable)"); }
    }
}
