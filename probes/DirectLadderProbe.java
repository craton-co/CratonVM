import java.util.HashMap;
import java.util.Locale;
import java.util.Map;

/**
 * Drives the seven JDK-ONLY-WAVE2 §4 "thin direct call" ladders so their
 * policy gate is actually exercised.
 *
 * The recognition for these lives in `try_compile`'s compile-time bytecode
 * scan, so a triple only reaches `direct_native_helper` when a method
 * CONTAINING that call site is JIT-compiled. `JdkOnlyIcHotProbe` never
 * reaches them — it reports `jit_direct_native_binds = 0` — so it cannot
 * distinguish a gate that refuses from one that is never asked.
 *
 * Each loop is its own method so each gets its own compile.
 */
public class DirectLadderProbe {
    static final int N = 400_000;

    // Intrinsic per the kind map: Integer.valueOf(I), Integer.intValue()
    static long boxing() {
        long acc = 0;
        for (int i = 0; i < N; i++) {
            Integer boxed = Integer.valueOf(i & 1023);
            acc += boxed.intValue();
        }
        return acc;
    }

    // Bridge per the kind map: HashMap.put / HashMap.get
    static long maps() {
        Map<Integer, Integer> m = new HashMap<>();
        long acc = 0;
        for (int i = 0; i < N; i++) {
            int k = i & 255;
            m.put(k, i);
            Integer v = m.get(k);
            if (v != null) acc += v;
        }
        return acc;
    }

    // Intrinsic per the kind map: StringLatin1.toLowerCase, reached through
    // String.toLowerCase(Locale) — which is itself unregistered.
    static long lower() {
        String[] pool = {"ALPHA", "Beta", "GAMMA", "delta"};
        long acc = 0;
        for (int i = 0; i < N; i++) {
            acc += pool[i & 3].toLowerCase(Locale.ROOT).length();
        }
        return acc;
    }

    public static void main(String[] args) {
        long a = 0;
        // Several rounds so the tiering threshold is comfortably passed.
        for (int round = 0; round < 3; round++) {
            a += boxing();
            a += maps();
            a += lower();
        }
        System.out.println("DIRECTLADDER acc=" + a);
    }
}
