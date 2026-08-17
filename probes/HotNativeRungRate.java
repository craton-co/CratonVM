import java.lang.ref.Reference;
import java.util.Objects;

/**
 * The two rungs the `--dump-native-registry` invocation census put at the top
 * of a `ByteBuffer` accessor: `Preconditions.checkIndex` (reached through the
 * public `Objects.checkIndex`) and `Reference.reachabilityFence`.
 *
 * Both are registered natives on this VM, so each call pays the ~160 ns generic
 * native funnel; both are also statically bound with a trivial body, which is
 * exactly the shape a thin `*_DIRECT_FN` helper serves. Every arm is its own
 * small method called REPS times — never a loop inline in `main`, which on this
 * VM measures the interpreter.
 *
 * Measured 2026-08-17 (real-JDK mode, G1), before and after wiring
 * `jit_preconditions_check_index_direct` / `jit_reachability_fence_direct` into
 * all THREE compile doors:
 *
 *   rung                          HotSpot   before    after
 *   Objects.checkIndex             0.27 ns  352.41    19.70   (18x)
 *   Reference.reachabilityFence    0.28 ns  360.64    18.88   (19x)
 *   both in one loop               0.34 ns 1748.61    45.55   (38x)
 *
 * The `main` tail is not decoration: a fast path that got the bounds contract
 * wrong would still print these numbers. `charAt(9)` must stay
 * `StringIndexOutOfBoundsException` and `checkIndex(5,5)` plain
 * `IndexOutOfBoundsException` — that distinction comes from `Preconditions`'
 * three static formatters, which is exactly why the helper declines the
 * throwing case to the generic dispatcher instead of reimplementing it.
 *
 *   cratonvm --java-home <jdk> -cp <out> HotNativeRungRate 4000000 40
 */
public final class HotNativeRungRate {
    static long sink;
    static final Object O = new Object();

    static long a_empty(int n)      { long a=0; for (int i=0;i<n;i++) { a += i; } return a; }
    static long b_checkIndex(int n) { long a=0; for (int i=0;i<n;i++) { a += Objects.checkIndex(i & 0xFFFF, 0x10000); } return a; }
    static long c_fence(int n)      { long a=0; for (int i=0;i<n;i++) { Reference.reachabilityFence(O); a += i; } return a; }
    static long d_both(int n)       { long a=0; for (int i=0;i<n;i++) { a += Objects.checkIndex(i & 0xFFFF, 0x10000); Reference.reachabilityFence(O); } return a; }

    interface Arm { long run(int n); }

    static void time(String name, Arm arm, int n, int reps) {
        int per = n / reps;
        for (int w = 0; w < reps; w++) { sink += arm.run(per); }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) { sink += arm.run(per); }
        long t1 = System.nanoTime();
        System.out.printf("%-28s %9.2f ns/op%n", name, (double) (t1 - t0) / (per * (long) reps));
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        time("empty loop",              HotNativeRungRate::a_empty,      n, reps);
        time("Objects.checkIndex",      HotNativeRungRate::b_checkIndex, n, reps);
        time("Reference.reachabilityFence", HotNativeRungRate::c_fence,  n, reps);
        time("both",                    HotNativeRungRate::d_both,       n, reps);
        System.out.println("sink=" + sink);
        // Correctness: the throwing contract must be untouched by any fast path.
        try {
            Objects.checkIndex(5, 5);
            System.out.println("checkIndex(5,5)=NO-THROW  <-- WRONG");
        } catch (Throwable t) {
            System.out.println("checkIndex(5,5)=" + t.getClass().getName());
        }
        try {
            Objects.checkIndex(-1, 5);
            System.out.println("checkIndex(-1,5)=NO-THROW  <-- WRONG");
        } catch (Throwable t) {
            System.out.println("checkIndex(-1,5)=" + t.getClass().getName());
        }
        try {
            "abc".charAt(9);
            System.out.println("\"abc\".charAt(9)=NO-THROW  <-- WRONG");
        } catch (Throwable t) {
            System.out.println("\"abc\".charAt(9)=" + t.getClass().getName());
        }
        System.out.println("checkIndex(0,1)=" + Objects.checkIndex(0, 1)
                + " checkIndex(4,5)=" + Objects.checkIndex(4, 5));
    }
}
