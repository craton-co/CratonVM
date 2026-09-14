package cratonvm;

/**
 * Regression for the JIT div-by-zero direct-throw fix.
 *
 * The old x64 idiv/irem/ldiv/lrem zero-divisor guard deopted via
 * {@code jit_uncommon_trap}, which re-ran the WHOLE method from entry in the
 * interpreter — so any side effect preceding the trap executed TWICE, diverging
 * from HotSpot. The fix direct-throws {@code ArithmeticException} through the
 * method's exception table (mirroring the bounds-check path), executing the
 * side effect exactly once.
 *
 * The per-call side effect is a heap-array increment ({@code c[0]++}) rather than
 * a static field, because the side effect lives in the SAME method as the divide
 * (so that method must be JIT'd to exercise the bug) and a static counter read
 * back from a separate interpreted method is not a reliable cross-tier probe.
 *
 * {@code *Delta} warms its divide method past the JIT threshold (each warm call
 * uses a non-zero divisor and resets {@code c[0]} so the marker does not
 * accumulate), then makes ONE divide-by-zero call. The single trapping call's
 * pre-divide increment must run exactly once: delta 1 (HotSpot / fixed), 2
 * (buggy uncommon-trap re-run).
 */
public class JitDivByZero {
    static final int[] c = new int[1];

    // Each method does the side effect (c[0]++) BEFORE the divide, in the SAME
    // method — the shape the bug corrupts. divisor!=0 on warm calls, 0 on the
    // single trapping call.
    static int idivStep(int a, int b) { c[0]++; return a / b; }
    static int iremStep(int a, int b) { c[0]++; return a % b; }
    static long ldivStep(long a, long b) { c[0]++; return a / b; }
    static long lremStep(long a, long b) { c[0]++; return a % b; }

    private static int delta(int op) {
        long sink = 0;
        // Warm: tier-up the step method. c[0] is reset every call so it never
        // accumulates; only the final trapping call's increment is measured.
        for (int i = 0; i < 20000; i++) {
            c[0] = 0;
            switch (op) {
                case 0: sink += idivStep(i, 7); break;
                case 1: sink += iremStep(i, 7); break;
                case 2: sink += ldivStep(i, 7L); break;
                default: sink += lremStep(i, 7L); break;
            }
        }
        c[0] = 0;
        try {
            switch (op) {
                case 0: idivStep(1, 0); break;
                case 1: iremStep(1, 0); break;
                case 2: ldivStep(1L, 0L); break;
                default: lremStep(1L, 0L); break;
            }
        } catch (ArithmeticException e) {}
        return (sink == -1) ? -1 : c[0];
    }

    public static int idivDelta() { return delta(0); }
    public static int iremDelta() { return delta(1); }
    public static int ldivDelta() { return delta(2); }
    public static int lremDelta() { return delta(3); }

    /**
     * Catch-in-method message correctness for a JIT'd divide-by-zero.
     * Returns 1 iff the caught ArithmeticException carries the HotSpot message
     * "/ by zero"; 2 if caught with a different message; 0 if not thrown.
     */
    public static int idivMessageOk() {
        long sink = 0;
        for (int i = 0; i < 20000; i++) { c[0] = 0; sink += idivStep(i, 7); }
        if (sink == -1) return -1;
        c[0] = 0;
        try {
            idivStep(1, 0);
            return 0;
        } catch (ArithmeticException e) {
            return "/ by zero".equals(e.getMessage()) ? 1 : 2;
        }
    }
}
