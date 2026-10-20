// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 64, lane L7: the `ldc` arm answers a warm int or
// float inline (`CRATONVM_INTERP_LDC_INLINE_HIT`, default off; proposal
// docs/internal/fixed-bugs/interpreter-i63-L7-proposal-the-ldc-arm-answers-a-warm-int-inline-DONE-20261027.md),
// and the fused int compare-and-branch takes a quickened `getstatic` int
// bound (`CRATONVM_INTERP_FUSED_GETSTATIC_BOUND`, default off; proposal
// i63-L7-proposal-the-fused-compare-and-branch-takes-a-getstatic-int-bound-20261027.md).
// No behaviour change: every line is the same under every combination of the
// two switches (and of wave 63's CRATONVM_INTERP_FUSED_LDC_BOUND /
// CRATONVM_INTERP_LDC_HOISTED_PC), with and without --nojit.
//
// Shapes:
//   getstatic  `x < bound` .. `x != bound` with `bound` a non-final static
//              int (`iload_0; getstatic; if_icmp<cond>`), 3000 rounds over
//              edge values, one mask per value; loops bounded by an int, a
//              short, a byte (`<=`), a char and a volatile int static; a
//              loop whose body shrinks its own static bound (the field must
//              be re-read every iteration: nothing but the site is
//              memoized); the same loop after the bound was reassigned
//              between calls; a bound in another class (its first
//              `getstatic` initializes it); a class whose initializer throws
//              (the fusion never answers an unfilled site, so the first
//              access raises ExceptionInInitializerError and the second
//              NoClassDefFoundError, as unfused).
//   ldc        `s += 100_003` (an int constant in a loop body), the same on
//              a fresh thread (an empty per-thread table: the first `ldc`
//              misses and the call fills it), `f += 0.75f` (a float), a mask
//              past `sipush`'s range (`i & 0x1FFFF`), and a String / Class
//              `ldc` in a loop (never held in the numeric table: the call
//              answers).
//
// Positive control (debug cratonvm, --nojit, CRATONVM_DBG_FIELD_SITE=1,
// CRATONVM_DBG_GETSTATIC_PROF=1 and CRATONVM_QUICKEN_STATS=pairs): with
// CRATONVM_INTERP_FUSED_GETSTATIC_BOUND=1 the `getstatic -> if_icmp*` pairs
// drop to the first execution of each site while the static-site get hits
// stay about the same (a fused hit counts as the quickened hit it replaces);
// with CRATONVM_INTERP_LDC_INLINE_HIT=1 `ldc: hit=` stays the same (the
// inline hit counts as the call's hit).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// getstatic -2147483648 35
// getstatic -1 35
// getstatic 69999 35
// getstatic 70000 26
// getstatic 70001 44
// getstatic 2147483647 44
// getstatic total 657000
// loop int-bound 450000
// loop short-bound 20000
// loop byte-bound-le 101
// loop char-bound 4950
// loop volatile-bound 250000
// loop shrinking-bound 5000500
// loop reassigned-bound 9
// loop other-class-bound 12345
// bad first java.lang.ExceptionInInitializerError
// bad second java.lang.NoClassDefFoundError
// ldc int-body -1474236480
// ldc int-body-thread -1474236480
// ldc float-body 750.0
// ldc wide-mask 499500
// ldc string-body 5000
// ldc class-body java.lang.Integer
// HOTSPOT_EXPECTED_END
public class L7W64LdcInlineGetstaticBound {
    static int bound = 70000;
    static int count = 300_000;
    static short sCount = 20_000;
    static byte bCount = 100;
    static char cCount = 'd';
    static volatile int vCount = 250_000;
    static int shrinking = 1000;

    static class Other {
        static int n;

        static {
            n = 12345;
        }
    }

    static class Bad {
        static int n;

        static {
            if (alwaysTrue()) {
                throw new RuntimeException("boom");
            }
            n = 1;
        }
    }

    static boolean alwaysTrue() {
        return true;
    }

    static int conds(int x) {
        int m = 0;
        if (x < bound) m |= 1;
        if (x <= bound) m |= 2;
        if (x > bound) m |= 4;
        if (x >= bound) m |= 8;
        if (x == bound) m |= 16;
        if (x != bound) m |= 32;
        return m;
    }

    static int intBound() {
        int s = 0;
        for (int i = 0; i < count; i++) {
            s += i & 3;
        }
        return s;
    }

    static int shortBound() {
        int n = 0;
        for (int i = 0; i < sCount; i++) {
            n++;
        }
        return n;
    }

    static int byteBoundLe() {
        int n = 0;
        for (int i = 0; i <= bCount; i++) {
            n++;
        }
        return n;
    }

    static int charBound() {
        int s = 0;
        for (int i = 0; i < cCount; i++) {
            s += i;
        }
        return s;
    }

    static int volatileBound() {
        int n = 0;
        for (int i = 0; i < vCount; i++) {
            n++;
        }
        return n;
    }

    static int shrinkingBound() {
        int n = 0;
        for (int i = 0; i < shrinking; i++) {
            shrinking--;
            n++;
        }
        return n * 10000 + shrinking;
    }

    static int otherClassBound() {
        int n = 0;
        for (int i = 0; i < Other.n; i++) {
            n++;
        }
        return n;
    }

    static int badBound() {
        int n = 0;
        for (int i = 0; i < Bad.n; i++) {
            n++;
        }
        return n;
    }

    static int ldcBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += 100_003;
        }
        return s;
    }

    static float floatBody() {
        float f = 0;
        for (int i = 0; i < 1000; i++) {
            f += 0.75f;
        }
        return f;
    }

    static int wideMask() {
        int s = 0;
        for (int i = 0; i < 1000; i++) {
            s += i & 0x1FFFF;
        }
        return s;
    }

    static int stringBody() {
        int n = 0;
        for (int i = 0; i < 1000; i++) {
            n += "hello".length();
        }
        return n;
    }

    static String classBody() {
        String name = null;
        for (int i = 0; i < 1000; i++) {
            name = Integer.class.getName();
        }
        return name;
    }

    public static void main(String[] args) throws Exception {
        final int[] values = {Integer.MIN_VALUE, -1, 69999, 70000, 70001, Integer.MAX_VALUE};
        int[] masks = new int[values.length];
        long total = 0;
        for (int r = 0; r < 3000; r++) {
            for (int k = 0; k < values.length; k++) {
                masks[k] = conds(values[k]);
                total += masks[k];
            }
        }
        for (int k = 0; k < values.length; k++) {
            System.out.println("getstatic " + values[k] + " " + masks[k]);
        }
        System.out.println("getstatic total " + total);
        System.out.println("loop int-bound " + intBound());
        System.out.println("loop short-bound " + shortBound());
        System.out.println("loop byte-bound-le " + byteBoundLe());
        System.out.println("loop char-bound " + charBound());
        System.out.println("loop volatile-bound " + volatileBound());
        System.out.println("loop shrinking-bound " + shrinkingBound());
        count = 7;
        System.out.println("loop reassigned-bound " + intBound());
        System.out.println("loop other-class-bound " + otherClassBound());
        try {
            badBound();
            System.out.println("bad first no exception");
        } catch (Throwable t) {
            System.out.println("bad first " + t.getClass().getName());
        }
        try {
            badBound();
            System.out.println("bad second no exception");
        } catch (Throwable t) {
            System.out.println("bad second " + t.getClass().getName());
        }
        System.out.println("ldc int-body " + ldcBody(200_000));
        int[] fromThread = new int[1];
        Thread t = new Thread(() -> fromThread[0] = ldcBody(200_000));
        t.start();
        t.join();
        System.out.println("ldc int-body-thread " + fromThread[0]);
        System.out.println("ldc float-body " + floatBody());
        System.out.println("ldc wide-mask " + wideMask());
        System.out.println("ldc string-body " + stringBody());
        System.out.println("ldc class-body " + classBody());
    }
}
