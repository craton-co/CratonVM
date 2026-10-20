// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 65, lane L7: the `getstatic` arm answers a
// quickened site inline (`CRATONVM_INTERP_GETSTATIC_INLINE_HIT`, default off;
// proposal
// docs/internal/fixed-bugs/interpreter-i64-L7-proposal-the-getstatic-arm-answers-a-quickened-site-inline-DONE-20261028.md),
// and the `ldc2_w` arm answers a warm long or double inline
// (`CRATONVM_INTERP_LDC2W_INLINE_HIT`, default off; proposal
// i64-L7-proposal-the-ldc2w-arm-answers-a-warm-long-or-double-inline-20261028.md).
// No behaviour change: every line is the same under every combination of the
// two switches (and of wave 62's CRATONVM_INTERP_HOISTED_OBSERVERS, wave 64's
// CRATONVM_INTERP_LDC_INLINE_HIT / CRATONVM_INTERP_FUSED_GETSTATIC_BOUND),
// with and without --nojit.
//
// Shapes:
//   getstatic  every descriptor the arm pushes, read in a loop body: an int
//              counter, a boolean flag, byte / short / char (negative and
//              high values keep their sign or zero extension), a float, a
//              long whose top bits collide with the NaN-box tag space, a
//              double with a non-canonical NaN payload (both printed as raw
//              bits), a double coefficient, never-assigned long / double
//              statics (zero-initialized slots), a String, a null reference
//              and an int[] table; a static the loop's callee changes (read
//              afresh every iteration); a volatile static; a static read
//              through a subclass (only the declaring class initializes); a
//              class whose first `getstatic` initializes it (its initializer
//              prints first); a class whose initializer throws
//              (ExceptionInInitializerError, then NoClassDefFoundError: an
//              unfilled site is never answered inline).
//   ldc2_w     a hash multiplier, a negative long and a 32-bit mask, a
//              double coefficient, a long loop bound and step, a long
//              constant that collides with the tag space, the canonical NaN,
//              negative zero and negative infinity (raw bits), the same hash
//              on a fresh thread (an empty per-thread table: the first
//              `ldc2_w` misses and the call fills it).
//
// Positive control (debug cratonvm, --nojit, CRATONVM_DBG_GETSTATIC_PROF=1):
// with CRATONVM_INTERP_GETSTATIC_INLINE_HIT=1 the static-site get-hit count
// stays the same (an inline hit counts as the quickened hit it replaces).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// getstatic int-body 3000
// getstatic boolean-body 1000
// getstatic sub-int-body -24200
// getstatic float-body 250.0
// getstatic long-bits fffc0000deadbeef
// getstatic double-bits 7ff80000deadbeef
// getstatic double-coeff 249750.0
// getstatic zero-long 0
// getstatic zero-double 0.0
// getstatic string-body 3000
// getstatic null-body 1000
// getstatic table-body 2500
// getstatic changing 55
// getstatic volatile-body 4000
// init Base
// getstatic inherited 70
// init Lazy
// getstatic lazy 33
// bad first java.lang.ExceptionInInitializerError
// bad second java.lang.NoClassDefFoundError
// ldc2w hash aeb29957cb295bec
// ldc2w hash-thread aeb29957cb295bec
// ldc2w mask 4290668832204
// ldc2w double-coeff 249750.0
// ldc2w long-bound 3
// ldc2w collision fffc0000deadbeef
// ldc2w nan 7ff8000000000000
// ldc2w neg-zero 8000000000000000
// ldc2w neg-inf fff0000000000000
// ldc2w max-long 7fffffffffffffff
// HOTSPOT_EXPECTED_END
public class L7W65GetstaticLdc2wInline {
    static int step = 3;
    static boolean flag = true;
    static byte b = -7;
    static short sh = -300;
    static char ch = 'A';
    static float quarter = 0.25f;
    static long collision = 0xFFFC0000DEADBEEFL;
    static double nanPayload = Double.longBitsToDouble(0x7FF80000DEADBEEFL);
    static double half = 0.5;
    static long zeroLong;
    static double zeroDouble;
    static String name = "abc";
    static Object nothing;
    static int[] table = {1, 2, 3, 4};
    static int changing = 1;
    static volatile int vstep = 4;

    static class Base {
        static int baseVal = 7;

        static {
            System.out.println("init Base");
        }
    }

    static class Sub extends Base {
        static {
            System.out.println("init Sub");
        }
    }

    static class Lazy {
        static int v = 11;

        static {
            System.out.println("init Lazy");
        }
    }

    static class Bad {
        static long n;

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

    static int intBody() {
        int s = 0;
        for (int i = 0; i < 1000; i++) {
            s += step;
        }
        return s;
    }

    static int booleanBody() {
        int n = 0;
        for (int i = 0; i < 1000; i++) {
            if (flag) {
                n++;
            }
        }
        return n;
    }

    static int subIntBody() {
        int s = 0;
        for (int i = 0; i < 100; i++) {
            s += b + sh + ch;
        }
        return s;
    }

    static float floatBody() {
        float f = 0;
        for (int i = 0; i < 1000; i++) {
            f += quarter;
        }
        return f;
    }

    static long longBits() {
        long acc = 0;
        for (int i = 0; i < 101; i++) {
            acc ^= collision;
        }
        return acc;
    }

    static double doubleBits() {
        double d = 0;
        for (int i = 0; i < 100; i++) {
            d = nanPayload;
        }
        return d;
    }

    static double doubleCoeff() {
        double d = 0;
        for (int i = 0; i < 1000; i++) {
            d += i * half;
        }
        return d;
    }

    static long zeroLongBody() {
        long s = 0;
        for (int i = 0; i < 100; i++) {
            s += zeroLong;
        }
        return s;
    }

    static double zeroDoubleBody() {
        double s = 0;
        for (int i = 0; i < 100; i++) {
            s += zeroDouble;
        }
        return s;
    }

    static int stringBody() {
        int n = 0;
        for (int i = 0; i < 1000; i++) {
            n += name.length();
        }
        return n;
    }

    static int nullBody() {
        int n = 0;
        for (int i = 0; i < 1000; i++) {
            if (nothing == null) {
                n++;
            }
        }
        return n;
    }

    static int tableBody() {
        int s = 0;
        for (int i = 0; i < 1000; i++) {
            s += table[i & 3];
        }
        return s;
    }

    static void bump() {
        changing++;
    }

    static int changingBody() {
        int s = 0;
        for (int i = 0; i < 10; i++) {
            s += changing;
            bump();
        }
        return s;
    }

    static int volatileBody() {
        int s = 0;
        for (int i = 0; i < 1000; i++) {
            s += vstep;
        }
        return s;
    }

    static int inherited() {
        int s = 0;
        for (int i = 0; i < 10; i++) {
            s += Sub.baseVal;
        }
        return s;
    }

    static int lazy() {
        int s = 0;
        for (int i = 0; i < 3; i++) {
            s += Lazy.v;
        }
        return s;
    }

    static long badBody() {
        long s = 0;
        for (int i = 0; i < 3; i++) {
            s += Bad.n;
        }
        return s;
    }

    static long hash() {
        long h = 0;
        for (int i = 0; i < 1000; i++) {
            h = h * 0x9E3779B97F4A7C15L + i;
        }
        return h;
    }

    static long mask() {
        long s = 0;
        for (int i = 0; i < 1000; i++) {
            s += ((long) i * -7L) & 0xFFFFFFFFL;
        }
        return s;
    }

    static double ldcDoubleCoeff() {
        double d = 0;
        for (int i = 0; i < 1000; i++) {
            d += i * 0.5;
        }
        return d;
    }

    static int longBound() {
        int n = 0;
        for (long i = 0; i < 3_000_000_000L; i += 1_000_000_000L) {
            n++;
        }
        return n;
    }

    static long ldcCollision() {
        long acc = 0;
        for (int i = 0; i < 101; i++) {
            acc ^= 0xFFFC0000DEADBEEFL;
        }
        return acc;
    }

    static double ldcNan() {
        double d = 0;
        for (int i = 0; i < 100; i++) {
            d = Double.NaN;
        }
        return d;
    }

    static double ldcNegZero() {
        double d = 1;
        for (int i = 0; i < 100; i++) {
            d = -0.0;
        }
        return d;
    }

    static double ldcNegInf() {
        double d = 0;
        for (int i = 0; i < 100; i++) {
            d = Double.NEGATIVE_INFINITY;
        }
        return d;
    }

    static long ldcMaxLong() {
        long x = 0;
        for (int i = 0; i < 100; i++) {
            x = Long.MAX_VALUE;
        }
        return x;
    }

    static String bits(double d) {
        return Long.toHexString(Double.doubleToRawLongBits(d));
    }

    public static void main(String[] args) throws Exception {
        System.out.println("getstatic int-body " + intBody());
        System.out.println("getstatic boolean-body " + booleanBody());
        System.out.println("getstatic sub-int-body " + subIntBody());
        System.out.println("getstatic float-body " + floatBody());
        System.out.println("getstatic long-bits " + Long.toHexString(longBits()));
        System.out.println("getstatic double-bits " + bits(doubleBits()));
        System.out.println("getstatic double-coeff " + doubleCoeff());
        System.out.println("getstatic zero-long " + zeroLongBody());
        System.out.println("getstatic zero-double " + zeroDoubleBody());
        System.out.println("getstatic string-body " + stringBody());
        System.out.println("getstatic null-body " + nullBody());
        System.out.println("getstatic table-body " + tableBody());
        System.out.println("getstatic changing " + changingBody());
        System.out.println("getstatic volatile-body " + volatileBody());
        int inh = inherited();
        System.out.println("getstatic inherited " + inh);
        int lz = lazy();
        System.out.println("getstatic lazy " + lz);
        try {
            badBody();
            System.out.println("bad first no exception");
        } catch (Throwable t) {
            System.out.println("bad first " + t.getClass().getName());
        }
        try {
            badBody();
            System.out.println("bad second no exception");
        } catch (Throwable t) {
            System.out.println("bad second " + t.getClass().getName());
        }
        System.out.println("ldc2w hash " + Long.toHexString(hash()));
        long[] fromThread = new long[1];
        Thread t = new Thread(() -> fromThread[0] = hash());
        t.start();
        t.join();
        System.out.println("ldc2w hash-thread " + Long.toHexString(fromThread[0]));
        System.out.println("ldc2w mask " + mask());
        System.out.println("ldc2w double-coeff " + ldcDoubleCoeff());
        System.out.println("ldc2w long-bound " + longBound());
        System.out.println("ldc2w collision " + Long.toHexString(ldcCollision()));
        System.out.println("ldc2w nan " + bits(ldcNan()));
        System.out.println("ldc2w neg-zero " + bits(ldcNegZero()));
        System.out.println("ldc2w neg-inf " + bits(ldcNegInf()));
        System.out.println("ldc2w max-long " + Long.toHexString(ldcMaxLong()));
    }
}
