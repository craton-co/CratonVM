// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 94, lane L3 (`CRATONVM_PUBLIC_METHOD_JDK_WALK`,
// i52-L3-proposal-one-public-method-walk-for-getmethod-and-getmethods): what
// do the served `Class.getMethods()` / `getMethod(..)` cost with JDK 25's
// walk (`privateGetPublicMethods` / `getMethodsRecursive`, recursion with a
// per-call memo and per-key merging) against the older depth-first walks?
// The page asks that the merged walk not be slower than the stack walk.
//
//   methods-arraylist   ArrayList.class.getMethods()     (wide: ~60 public methods,
//                       5 superclass/interface levels, List reached twice)
//   methods-hashmap     HashMap.class.getMethods()
//   methods-deep        getMethods() of a 12-level class chain, each level
//                       implementing a small interface pair
//   method-inherited    Deep12.class.getMethod("level0") (found on the last rung;
//                       the link-resolver cache answers after the first call,
//                       so this row measures the cached door and must not move)
//   method-miss         ArrayList.class.getMethod("nope") (a full walk and a
//                       NoSuchMethodException each time)
//
// The served walks answer only where the Bridge runs (`--compatible`; under
// `--jdk-only` the JDK bodies of `getMethods` / `getMethod` usually run, and
// the rows must not move there).
//
// Output: "row <name> min <ns> med <ns> ns/op" and "checksum <name> <n>".
// Run: javac -d . L3W94PublicMethodWalkBench.java
//      java -cp . L3W94PublicMethodWalkBench                                      (HotSpot)
//      cratonvm --compatible -cp . L3W94PublicMethodWalkBench
//      CRATONVM_PUBLIC_METHOD_JDK_WALK=1 cratonvm --compatible -cp . L3W94PublicMethodWalkBench
//      (and --nojit, and --jdk-only; interleave 5x)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; checksums only, derived from the class shapes; methods-* counts are JDK-version dependent and are to be measured)
// checksum method-inherited 1
// checksum method-miss 1
// HOTSPOT_EXPECTED_END
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Locale;

public class L3W94PublicMethodWalkBench {
    public interface P0 { default int p0() { return 0; } }
    public interface Q0 extends P0 { default int q0() { return 0; } }
    public static class Deep0 implements P0, Q0 { public int level0() { return 0; } }
    public static class Deep1 extends Deep0 implements Q0 { public int level1() { return 1; } }
    public static class Deep2 extends Deep1 implements P0 { public int level2() { return 2; } }
    public static class Deep3 extends Deep2 implements Q0 { public int level3() { return 3; } }
    public static class Deep4 extends Deep3 implements P0 { public int level4() { return 4; } }
    public static class Deep5 extends Deep4 implements Q0 { public int level5() { return 5; } }
    public static class Deep6 extends Deep5 implements P0 { public int level6() { return 6; } }
    public static class Deep7 extends Deep6 implements Q0 { public int level7() { return 7; } }
    public static class Deep8 extends Deep7 implements P0 { public int level8() { return 8; } }
    public static class Deep9 extends Deep8 implements Q0 { public int level9() { return 9; } }
    public static class Deep10 extends Deep9 implements P0 { public int level10() { return 10; } }
    public static class Deep11 extends Deep10 implements Q0 { public int level11() { return 11; } }
    public static class Deep12 extends Deep11 implements P0 { public int level12() { return 12; } }

    interface Op { long run() throws Exception; }

    static void bench(String name, int ops, Op op) throws Exception {
        long check = 0;
        for (int i = 0; i < ops; i++) {
            check = op.run();
        }
        long[] t = new long[9];
        for (int r = 0; r < t.length; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < ops; i++) {
                check = op.run();
            }
            t[r] = (System.nanoTime() - t0) / ops;
        }
        Arrays.sort(t);
        System.out.println(String.format(Locale.ROOT, "row %s min %d med %d ns/op", name, t[0], t[t.length / 2]));
        System.out.println("checksum " + name + " " + check);
    }

    public static void main(String[] args) throws Exception {
        int ops = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        bench("methods-arraylist", ops, () -> ArrayList.class.getMethods().length);
        bench("methods-hashmap", ops, () -> HashMap.class.getMethods().length);
        bench("methods-deep", ops, () -> Deep12.class.getMethods().length);
        bench("method-inherited", ops, () -> Deep12.class.getMethod("level0").getDeclaringClass() == Deep0.class ? 1 : 0);
        bench("method-miss", ops / 4, () -> {
            try {
                ArrayList.class.getMethod("nope");
                return 0;
            } catch (NoSuchMethodException e) {
                return 1;
            }
        });
    }
}
