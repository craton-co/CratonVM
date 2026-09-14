package cratonvm;

/**
 * {@code ArrayIndexOutOfBoundsException} raised by COMPILED code, in the three
 * shapes that reach the JIT's bounds check by different routes.
 *
 * <p>A JIT bounds check does not necessarily throw where it happens. The
 * direct-throw stub ({@code jit_throw_aioobe}) flags a signal, returns the
 * {@code i64::MIN} deopt sentinel and runs the method EPILOGUE; the void-return
 * store helpers flag it and return normally. Either way the throwable is
 * constructed later, and if the compiled frames have already left the stack by
 * then, {@code fillInStackTrace} has nothing to walk. That is the defect fixed
 * for {@code NullPointerException} (2026-09-02) and for
 * {@code ArithmeticException} (2026-09-05); this fixture is what asks the same
 * question of the third signal.
 *
 * <p>Each entry point is driven by exactly one measurement. Sharing one between
 * a cold reading and a warm one is what silently turned
 * {@code pgo02_guarded_virtual_inline}'s "interpreted" control into a second
 * compiled reading.
 */
public class CompiledAioobeTrace {

    /** Length 4, so index 9 is out of bounds and 0..3 are not. */
    static final int[] DATA = new int[4];

    static class Indexer {
        final int[] data = new int[4];

        public int at(int i) {
            return data[i];
        }
    }

    static final Indexer INDEXER = new Indexer();

    /** The load, with NO callee between it and the entry point. */
    public static int directAtForTrace(int x) {
        return DATA[x];
    }

    /**
     * The load behind a virtual call on a static-final receiver — the shape
     * that lost its frames for {@code ArithmeticException}, where the callee is
     * small enough to be spliced into the caller's compiled body.
     */
    public static int callAtForTrace(int x) {
        return INDEXER.at(x);
    }

    /**
     * The STORE, which reaches the bounds check through a different helper
     * family: the void-return store helpers flag the signal and return normally
     * rather than running the epilogue.
     */
    public static int storeAtForTrace(int x) {
        DATA[x] = x;
        return DATA[0];
    }
}
