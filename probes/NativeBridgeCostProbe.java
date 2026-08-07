import java.util.Objects;

/**
 * Prices ONE registered-native ("bridge") call against the identical body run
 * as ordinary bytecode, on the same VM in the same process.
 *
 * CratonVM registers a Rust native for many trivial JDK methods and, via
 * `should_force_registered_native_over_bytecode` or the interpreter's
 * native-preferred dispatch, runs the native instead of the real bytecode. That
 * is a win when the native replaces real work (a syscall, a decompress) and a
 * LOSS when the body is five bytecodes, because the call has to cross
 * `safe_native_call`: marshal every argument into a `Vec<Value>`, validate each
 * one against the heap, dispatch through the registry, and coerce the return.
 *
 * Each rung below has a bridged version and a `mine*` twin with a
 * byte-for-byte equivalent body that CratonVM has no registration for, so the
 * twin necessarily runs as bytecode. The DIFFERENCE between the two columns is
 * the bridge's price; the twin's own column is the interpreter/JIT floor and is
 * not interesting on its own.
 *
 * Read the CratonVM ratio (bridged / mine) and compare it with HotSpot's, where
 * both columns are the same kind of Java call and the ratio must be ~1.
 *
 * Usage: NativeBridgeCostProbe [iterations] [rounds]
 */
public final class NativeBridgeCostProbe {

    static final String MSG = "nope";
    static final Object OBJ = new Object();
    static final Object[] ARR = new Object[] {OBJ, MSG, Integer.valueOf(7)};

    // ---- requireNonNull: 1.73M calls in a Tomcat webapp deploy -------------

    static Object mineRequireNonNull(Object o, String m) {
        if (o == null) {
            throw new NullPointerException(m);
        }
        return o;
    }

    static long bridgedRequireNonNull(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += Objects.requireNonNull(OBJ, MSG).hashCode() >>> 24;
        }
        return acc;
    }

    static long mineRequireNonNullLoop(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += mineRequireNonNull(OBJ, MSG).hashCode() >>> 24;
        }
        return acc;
    }

    // ---- getClass: 35k calls, and it backs every isAssignableFrom check ----

    static long bridgedGetClass(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += ARR[i & 1].getClass().hashCode() >>> 24;
        }
        return acc;
    }

    static Class<?> mineGetClass(Object o) {
        return o == OBJ ? Object.class : String.class;
    }

    static long mineGetClassLoop(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += mineGetClass(ARR[i & 1]).hashCode() >>> 24;
        }
        return acc;
    }

    // ---- isAssignableFrom + cast: 886k calls, both inside BCEL's ----------
    // ---- ConstantPool.getConstant, once per constant-pool access ----------

    static final Class<?> OBJECT_CLASS = Object.class;

    static long bridgedAssignableCast(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            if (OBJECT_CLASS.isAssignableFrom(ARR[i & 1].getClass())) {
                acc += OBJECT_CLASS.cast(ARR[i & 1]) == null ? 1 : 2;
            }
        }
        return acc;
    }

    static boolean mineAssignable(Object o) {
        return o != null;
    }

    static long mineAssignableCastLoop(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            if (mineAssignable(ARR[i & 1])) {
                acc += ARR[i & 1] == null ? 1 : 2;
            }
        }
        return acc;
    }

    // ---- harness ----------------------------------------------------------

    static long sink;

    static double time(String what, java.util.function.IntToLongFunction f, int n) {
        long t0 = System.nanoTime();
        sink += f.applyAsLong(n);
        return (System.nanoTime() - t0) / (double) n;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 300000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        for (int r = 0; r < rounds; r++) {
            double a = time("rnn-bridged", NativeBridgeCostProbe::bridgedRequireNonNull, n);
            double b = time("rnn-mine", NativeBridgeCostProbe::mineRequireNonNullLoop, n);
            double c = time("getclass-bridged", NativeBridgeCostProbe::bridgedGetClass, n);
            double d = time("getclass-mine", NativeBridgeCostProbe::mineGetClassLoop, n);
            double e = time("asgncast-bridged", NativeBridgeCostProbe::bridgedAssignableCast, n);
            double f = time("asgncast-mine", NativeBridgeCostProbe::mineAssignableCastLoop, n);
            System.out.printf(
                    "round %d  requireNonNull %9.1f vs %9.1f (%5.1fx) | getClass %9.1f vs %9.1f (%5.1fx) | isAssignableFrom+cast %9.1f vs %9.1f (%5.1fx)  ns/op%n",
                    r, a, b, a / Math.max(b, 0.001), c, d, c / Math.max(d, 0.001), e, f, e / Math.max(f, 0.001));
        }
        System.out.println("sink=" + (sink == Long.MIN_VALUE ? 1 : 0));
    }
}
