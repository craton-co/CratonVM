/**
 * `Integer.valueOf`/`intValue` have thin `*_DIRECT_FN` binds in the JIT
 * (`INTEGER_VALUE_OF_DIRECT_FN` / `INTEGER_INT_VALUE_DIRECT_FN`).  `Long` has
 * neither, and `Long.valueOf(J)` / `Long.longValue()` are registered natives
 * on this VM just like the `Integer` pair is.
 *
 * Same binary, same run, same loop shape: if the Long rungs are several times
 * the Integer rungs, the gap is the missing bind and not boxing itself.
 * Values are chosen OUTSIDE the -128..127 cache on both arms so each rung
 * allocates, which is the expensive case the Integer bind was written for;
 * the cached arms are printed too so a reader can see both.
 *
 *   cratonvm --java-home <jdk> -cp <out> BoxRungRate 4000000 40
 */
public final class BoxRungRate {
    static long sink;
    static Object osink;

    static long a_empty(int n)       { long a = 0; for (int i = 0; i < n; i++) { a += i; } return a; }
    static long b_int_box(int n)     { long a = 0; for (int i = 0; i < n; i++) { Integer v = Integer.valueOf(i + 1000); a += v.intValue(); } return a; }
    static long c_long_box(int n)    { long a = 0; for (int i = 0; i < n; i++) { Long v = Long.valueOf(i + 1000L); a += v.longValue(); } return a; }
    static long d_int_cached(int n)  { long a = 0; for (int i = 0; i < n; i++) { Integer v = Integer.valueOf(i & 0x3F); a += v.intValue(); } return a; }
    static long e_long_cached(int n) { long a = 0; for (int i = 0; i < n; i++) { Long v = Long.valueOf(i & 0x3F); a += v.longValue(); } return a; }
    static long f_int_valueof(int n) { long a = 0; for (int i = 0; i < n; i++) { osink = Integer.valueOf(i + 1000); a += i; } return a; }
    static long g_long_valueof(int n){ long a = 0; for (int i = 0; i < n; i++) { osink = Long.valueOf(i + 1000L); a += i; } return a; }

    interface Arm { long run(int n); }

    static void time(String name, Arm arm, int n, int reps) {
        int per = n / reps;
        for (int r = 0; r < 3; r++) { sink += arm.run(per); }
        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            sink += arm.run(per);
            long dt = System.nanoTime() - t0;
            if (dt < best) { best = dt; }
        }
        System.out.printf("%-28s best=%9.2f ns/op%n", name, best / (double) per);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4000000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        time("empty (baseline)", BoxRungRate::a_empty, n, reps);
        time("Integer valueOf+intValue", BoxRungRate::b_int_box, n, reps);
        time("Long    valueOf+longValue", BoxRungRate::c_long_box, n, reps);
        time("Integer valueOf (cached)", BoxRungRate::d_int_cached, n, reps);
        time("Long    valueOf (cached)", BoxRungRate::e_long_cached, n, reps);
        time("Integer valueOf alone", BoxRungRate::f_int_valueof, n, reps);
        time("Long    valueOf alone", BoxRungRate::g_long_valueof, n, reps);
        System.out.println("sink=" + (sink == 0 ? 1 : 0) + " osink=" + (osink == null ? 1 : 0));
    }
}
