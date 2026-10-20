// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 83, lane L7: what the fused increments' `ldc2_w`
// long operand (CRATONVM_INTERP_FUSED_INCREMENT_LDC2W, default off;
// interpreter-i69-L7-proposal-ldc2w-long-operand-for-the-fused-increments-DONE-20261014) must
// leave unchanged: `getstatic J; ldc2_w; ladd | lsub; putstatic` (plain,
// wrapping, volatile, another class's static) and `aload_0; dup; getfield J;
// ldc2_w; ladd | lsub; [dup2_x1;] putfield` (the statement form and the
// pre-increment whose value is used), next to the shapes that must still
// decline (a `double` constant, an int field widened to long, a null
// receiver whose NPE is raised at the `getfield` after nine fused hits).
// Every loop runs long enough for the constant table to be warm.
//
// Run with the switch off and on (the wide switches and the ldc2_w inline
// hit are on by default); both must print the block below, with and without
// --nojit:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W83Ldc2wIncrement
//   CRATONVM_INTERP_FUSED_INCREMENT_LDC2W=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W83Ldc2wIncrement
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// staticAdd 1000000000
// staticSub -1024000
// staticWrap -9223372036854775806
// volatileAdd 1000007
// otherClass 10000
// fieldAdd 5000000000000
// fieldSub -2900
// fieldPre 3503500 n=7000
// fieldDouble 2500.0
// intPlusLong 3000
// npe done=9 n=81
// HOTSPOT_EXPECTED_END
public class L7W83Ldc2wIncrement {
    static final int ITERS = 1000;

    static long s;
    static volatile long vs;
    static long wrap;

    long n;
    double d;
    int m;

    static final class Other {
        static long c;
    }

    // getstatic J; ldc2_w 1000000; ladd; putstatic
    static long staticAdd() {
        s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += 1_000_000L;
        }
        return s;
    }

    // getstatic J; ldc2_w 1024; lsub; putstatic
    static long staticSub() {
        s = 0;
        for (int i = 0; i < ITERS; i++) {
            s -= 1024L;
        }
        return s;
    }

    // Wraps past Long.MAX_VALUE as `ladd` does.
    static long staticWrap() {
        wrap = Long.MAX_VALUE - 5;
        for (int i = 0; i < 4; i++) {
            wrap += 2L;
        }
        return wrap;
    }

    static long volatileAdd() {
        vs = 7;
        for (int i = 0; i < ITERS; i++) {
            vs += 1_000L;
        }
        return vs;
    }

    static long otherClass() {
        Other.c = 0;
        for (int i = 0; i < ITERS; i++) {
            Other.c += 10L;
        }
        return Other.c;
    }

    // aload_0; dup; getfield J; ldc2_w 5000000000; ladd; putfield
    long fieldAdd() {
        n = 0;
        for (int i = 0; i < ITERS; i++) {
            n += 5_000_000_000L;
        }
        return n;
    }

    long fieldSub() {
        n = 100;
        for (int i = 0; i < ITERS; i++) {
            n -= 3L;
        }
        return n;
    }

    // lload sum; aload_0; dup; getfield J; ldc2_w 7; ladd; dup2_x1;
    // putfield; ladd; lstore sum
    String fieldPre() {
        n = 0;
        long sum = 0;
        for (int i = 0; i < ITERS; i++) {
            sum += (n += 7L);
        }
        return sum + " n=" + n;
    }

    // ldc2_w of a CONSTANT_Double; dadd: never fused.
    double fieldDouble() {
        d = 0;
        for (int i = 0; i < ITERS; i++) {
            d += 2.5;
        }
        return d;
    }

    // getfield I; i2l; ldc2_w 3; ladd; l2i: never fused.
    int intPlusLong() {
        m = 0;
        for (int i = 0; i < ITERS; i++) {
            m += 3L;
        }
        return m;
    }

    // aload_0; dup; getfield J; ldc2_w 9; ladd; putfield
    static void bump(L7W83Ldc2wIncrement h) {
        h.n += 9L;
    }

    static String npe() {
        L7W83Ldc2wIncrement a = new L7W83Ldc2wIncrement();
        int done = 0;
        try {
            for (int i = 0; i < 10; i++) {
                bump(i < 9 ? a : null);
                done++;
            }
        } catch (NullPointerException e) {
            return "npe done=" + done + " n=" + a.n;
        }
        return "none done=" + done;
    }

    public static void main(String[] args) {
        L7W83Ldc2wIncrement t = new L7W83Ldc2wIncrement();
        System.out.println("staticAdd " + staticAdd());
        System.out.println("staticSub " + staticSub());
        System.out.println("staticWrap " + staticWrap());
        System.out.println("volatileAdd " + volatileAdd());
        System.out.println("otherClass " + otherClass());
        System.out.println("fieldAdd " + t.fieldAdd());
        System.out.println("fieldSub " + t.fieldSub());
        System.out.println("fieldPre " + t.fieldPre());
        System.out.println("fieldDouble " + t.fieldDouble());
        System.out.println("intPlusLong " + t.intPlusLong());
        System.out.println(npe());
    }
}
