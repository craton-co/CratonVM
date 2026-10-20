// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 66, lane L7: the `putstatic` arm stores to a
// quickened site inline (`CRATONVM_INTERP_PUTSTATIC_INLINE_HIT`, default off;
// proposal
// docs/internal/fixed-bugs/interpreter-i65-L7-proposal-the-putstatic-arm-stores-a-quickened-site-inline-DONE-20261029.md).
// No behaviour change: every line is the same with the switch off and on
// (and with wave 65's CRATONVM_INTERP_GETSTATIC_INLINE_HIT, wave 62's
// CRATONVM_INTERP_HOISTED_OBSERVERS), with and without --nojit.
//
// Shapes (each a `putstatic` executed in a loop body, so the site is warm):
//   an int counter (`counter++`), a long sequence (`seq += 3`), a long whose
//   top bits collide with the NaN-box tag space (KIND_LONG pop, raw bits), a
//   double with a non-canonical NaN payload (KIND_DOUBLE pop, raw bits), a
//   double accumulator, a double stored from `i2d`, byte / short / char
//   statics that wrap (`+=`), a boolean toggled an odd number of times, a
//   float accumulator, a String chosen per iteration, a reference stored as
//   null on the last iteration, a fresh int[] per iteration, a volatile
//   counter (then the same counter from another thread, after a join), a
//   static the loop's callee reads (each store visible to the next read), a
//   final static assigned in its own <clinit> (never a quickened site), a
//   static stored through a subclass (only the declaring class initializes),
//   a class whose first access is a `putstatic` (its initializer prints
//   first), a class whose initializer throws (ExceptionInInitializerError,
//   then NoClassDefFoundError: an unfilled site is never stored inline).
//
// Not covered here: a JDWP / JVMTI FieldModification watch armed mid-loop
// (needs a debugger driver; the arm declines on the same watch union the
// call's own quickened hit tests).
//
// Positive control (debug cratonvm, --nojit, CRATONVM_DBG_GETSTATIC_PROF=1):
// with CRATONVM_INTERP_PUTSTATIC_INLINE_HIT=1 the static-site put-hit count
// stays the same (an inline store counts as the quickened hit it replaces).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// putstatic int-counter 1000
// putstatic long-seq 3000
// putstatic long-bits fffc0000deadbe8c
// putstatic double-bits 7ff80000deadbeef
// putstatic double-total 500.0
// putstatic double-conv 999.0
// putstatic byte -56
// putstatic short -25536
// putstatic char Z
// putstatic boolean true
// putstatic float 250.0
// putstatic string even
// putstatic ref-null true
// putstatic table 999
// putstatic volatile 1000
// putstatic volatile-thread 2000
// putstatic changing 45
// putstatic final-clinit 45
// init Base
// putstatic inherited 17
// init LazyPut
// putstatic lazy 2
// bad first java.lang.ExceptionInInitializerError
// bad second java.lang.NoClassDefFoundError
// HOTSPOT_EXPECTED_END
public class L7W66PutstaticInline {
    static int counter;
    static long seq;
    static long lbits;
    static double dbits;
    static double dtotal;
    static double dconv;
    static byte bb;
    static short ss;
    static char cc = 'A';
    static boolean zz;
    static float ff;
    static String name;
    static Object ref = "start";
    static int[] holder;
    static volatile int vcount;
    static int changing;

    static final String[] NAMES = {"even", "odd"};

    static class Fin {
        static final long STAMP;

        static {
            long s = 0;
            for (int i = 0; i < 10; i++) {
                s += i;
            }
            STAMP = s;
        }
    }

    static class Base {
        static int baseCount = 7;

        static {
            System.out.println("init Base");
        }
    }

    static class Sub extends Base {
        static {
            System.out.println("init Sub");
        }
    }

    static class LazyPut {
        static int w;

        static {
            System.out.println("init LazyPut");
        }
    }

    static class BadPut {
        static long n;

        static {
            if (alwaysTrue()) {
                throw new RuntimeException("boom");
            }
        }
    }

    static boolean alwaysTrue() {
        return true;
    }

    static int readChanging() {
        return changing;
    }

    static void badBody() {
        for (int i = 0; i < 3; i++) {
            BadPut.n = i;
        }
    }

    static void volatileLoop() {
        for (int i = 0; i < 1000; i++) {
            vcount++;
        }
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 1000; i++) {
            counter++;
        }
        System.out.println("putstatic int-counter " + counter);

        for (int i = 0; i < 1000; i++) {
            seq += 3;
        }
        System.out.println("putstatic long-seq " + seq);

        for (int i = 0; i < 100; i++) {
            lbits = 0xFFFC0000DEADBEEFL ^ (long) i;
        }
        System.out.println("putstatic long-bits " + Long.toHexString(lbits));

        double payload = Double.longBitsToDouble(0x7FF80000DEADBEEFL);
        for (int i = 0; i < 100; i++) {
            dbits = payload;
        }
        System.out.println("putstatic double-bits "
                + Long.toHexString(Double.doubleToRawLongBits(dbits)));

        for (int i = 0; i < 1000; i++) {
            dtotal += 0.5;
        }
        System.out.println("putstatic double-total " + dtotal);

        for (int i = 0; i < 1000; i++) {
            dconv = i;
        }
        System.out.println("putstatic double-conv " + dconv);

        for (int i = 0; i < 200; i++) {
            bb += 1;
        }
        System.out.println("putstatic byte " + bb);

        for (int i = 0; i < 40; i++) {
            ss += 1000;
        }
        System.out.println("putstatic short " + ss);

        for (int i = 0; i < 25; i++) {
            cc += 1;
        }
        System.out.println("putstatic char " + cc);

        for (int i = 0; i < 1001; i++) {
            zz = !zz;
        }
        System.out.println("putstatic boolean " + zz);

        for (int i = 0; i < 1000; i++) {
            ff += 0.25f;
        }
        System.out.println("putstatic float " + ff);

        for (int i = 0; i < 1001; i++) {
            name = NAMES[i & 1];
        }
        System.out.println("putstatic string " + name);

        for (int i = 0; i < 1000; i++) {
            ref = ((i & 1) == 0) ? "x" : null;
        }
        System.out.println("putstatic ref-null " + (ref == null));

        for (int i = 0; i < 1000; i++) {
            holder = new int[] {i};
        }
        System.out.println("putstatic table " + holder[0]);

        volatileLoop();
        System.out.println("putstatic volatile " + vcount);
        Thread t = new Thread(L7W66PutstaticInline::volatileLoop);
        t.start();
        t.join();
        System.out.println("putstatic volatile-thread " + vcount);

        int s = 0;
        for (int i = 0; i < 10; i++) {
            changing = i;
            s += readChanging();
        }
        System.out.println("putstatic changing " + s);

        System.out.println("putstatic final-clinit " + Fin.STAMP);

        for (int i = 0; i < 10; i++) {
            Sub.baseCount++;
        }
        System.out.println("putstatic inherited " + Base.baseCount);

        for (int i = 0; i < 3; i++) {
            LazyPut.w = i;
        }
        System.out.println("putstatic lazy " + LazyPut.w);

        try {
            badBody();
            System.out.println("bad first no exception");
        } catch (Throwable e) {
            System.out.println("bad first " + e.getClass().getName());
        }
        try {
            badBody();
            System.out.println("bad second no exception");
        } catch (Throwable e) {
            System.out.println("bad second " + e.getClass().getName());
        }
    }
}
