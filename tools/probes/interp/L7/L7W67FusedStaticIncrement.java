// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 67, lane L7: the fused static int increment
// (`getstatic #F; <int operand>; iadd | isub; putstatic #F`;
// CRATONVM_INTERP_FUSED_STATIC_INCREMENT, default off; proposal
// docs/internal/fixed-bugs/interpreter-i66-L7-proposal-a-fused-static-int-increment-DONE-20261030.md).
// No behaviour change: every line is the same with the switch off and on
// (with the now-default CRATONVM_INTERP_GETSTATIC_INLINE_HIT and
// CRATONVM_INTERP_PUTSTATIC_INLINE_HIT, and with either of them =0, which
// disables the fusion), with and without --nojit.
//
// Shapes (each executed in a loop body, so the site is warm):
//   fused: `counter++` (iconst_1; iadd), `down--` (iconst_1; isub),
//   `plus += 7` (bipush), `big += 1000` (sipush), `neg += -5` (bipush of a
//   negative), `minus += -1` (iconst_m1), `subneg -= -128` (bipush -128),
//   `big2 += -1000` (sipush of a negative), `acc += i` (iload_N), `acc2 -= i`
//   with `i` past local 3 (iload n), a sum that wraps past
//   Integer.MAX_VALUE, a volatile counter (then the same counter from another
//   thread, after a join), a static a callee reads after each increment, a
//   static incremented through a subclass (only the declaring class
//   initializes), a class whose first access is an increment (its
//   initializer prints first), a class whose initializer throws
//   (ExceptionInInitializerError, then NoClassDefFoundError: an unfilled
//   site is never fused);
//   never fused (the unfused arms answer): `x = y + 1` (two pool indices),
//   `dbl += dbl` (the operand is a getstatic), byte / short / char statics
//   (an i2b / i2s / i2c sits before the putstatic), a long static.
//
// Not covered here: a JDWP / JVMTI FieldAccess or FieldModification watch
// armed mid-loop, and a breakpoint or single step inside the group (need a
// debugger driver; the arm declines on the same watch union and on
// `fusion_off`, as every fusion does).
//
// Positive control (debug cratonvm, --nojit, CRATONVM_DBG_GETSTATIC_PROF=1):
// with CRATONVM_INTERP_FUSED_STATIC_INCREMENT=1 the static-site get-hit and
// put-hit counts stay the same (a fused group counts one of each, the hits
// it replaces).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// inc counter 1000
// inc down -1000
// inc plus 7000
// inc big 100000
// inc neg -500
// inc minus -10
// inc subneg 1280
// inc big2 -10000
// inc acc 499500
// inc acc2 -499500
// inc wrap -2147483624
// inc volatile 1000
// inc volatile-thread 2000
// inc observed 55
// init Base
// inc inherited 17
// init LazyInc
// inc lazy 3
// bad first java.lang.ExceptionInInitializerError
// bad second java.lang.NoClassDefFoundError
// inc pair x=1000 y=999
// inc doubled 1024
// inc byte -56
// inc short -25536
// inc char Z
// inc long 3000
// HOTSPOT_EXPECTED_END
public class L7W67FusedStaticIncrement {
    static int counter;
    static int down;
    static int plus;
    static int big;
    static int neg;
    static int minus;
    static int subneg;
    static int big2;
    static int acc;
    static int acc2;
    static int wrap = Integer.MAX_VALUE - 5;
    static volatile int vcount;
    static int hits;
    static int x;
    static int y;
    static int dbl = 1;
    static byte bb;
    static short ss;
    static char cc = 'A';
    static long seq;

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

    static class LazyInc {
        static int w;

        static {
            System.out.println("init LazyInc");
        }
    }

    static class BadInc {
        static int n;

        static {
            if (alwaysTrue()) {
                throw new RuntimeException("boom");
            }
        }
    }

    static boolean alwaysTrue() {
        return true;
    }

    static int readHits() {
        return hits;
    }

    static void badBody() {
        for (int i = 0; i < 3; i++) {
            BadInc.n++;
        }
    }

    static void volatileLoop() {
        for (int i = 0; i < 1000; i++) {
            vcount++;
        }
    }

    // `i` is local 4: the subtraction's operand is `iload 4`.
    static void subLocal(int a, int b, int c, int d) {
        for (int i = 0; i < 1000; i++) {
            acc2 -= i;
        }
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 1000; i++) {
            counter++;
        }
        System.out.println("inc counter " + counter);

        for (int i = 0; i < 1000; i++) {
            down--;
        }
        System.out.println("inc down " + down);

        for (int i = 0; i < 1000; i++) {
            plus += 7;
        }
        System.out.println("inc plus " + plus);

        for (int i = 0; i < 100; i++) {
            big += 1000;
        }
        System.out.println("inc big " + big);

        for (int i = 0; i < 100; i++) {
            neg += -5;
        }
        System.out.println("inc neg " + neg);

        for (int i = 0; i < 10; i++) {
            minus += -1;
        }
        System.out.println("inc minus " + minus);

        for (int i = 0; i < 10; i++) {
            subneg -= -128;
        }
        System.out.println("inc subneg " + subneg);

        for (int i = 0; i < 10; i++) {
            big2 += -1000;
        }
        System.out.println("inc big2 " + big2);

        for (int i = 0; i < 1000; i++) {
            acc += i;
        }
        System.out.println("inc acc " + acc);

        subLocal(1, 2, 3, 4);
        System.out.println("inc acc2 " + acc2);

        for (int i = 0; i < 10; i++) {
            wrap += 3;
        }
        System.out.println("inc wrap " + wrap);

        volatileLoop();
        System.out.println("inc volatile " + vcount);
        Thread t = new Thread(L7W67FusedStaticIncrement::volatileLoop);
        t.start();
        t.join();
        System.out.println("inc volatile-thread " + vcount);

        int s = 0;
        for (int i = 0; i < 10; i++) {
            hits++;
            s += readHits();
        }
        System.out.println("inc observed " + s);

        for (int i = 0; i < 10; i++) {
            Sub.baseCount++;
        }
        System.out.println("inc inherited " + Base.baseCount);

        for (int i = 0; i < 3; i++) {
            LazyInc.w++;
        }
        System.out.println("inc lazy " + LazyInc.w);

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

        for (int i = 0; i < 1000; i++) {
            y = i;
            x = y + 1;
        }
        System.out.println("inc pair x=" + x + " y=" + y);

        for (int i = 0; i < 10; i++) {
            dbl += dbl;
        }
        System.out.println("inc doubled " + dbl);

        for (int i = 0; i < 200; i++) {
            bb++;
        }
        System.out.println("inc byte " + bb);

        for (int i = 0; i < 40; i++) {
            ss += 1000;
        }
        System.out.println("inc short " + ss);

        for (int i = 0; i < 25; i++) {
            cc++;
        }
        System.out.println("inc char " + cc);

        for (int i = 0; i < 1000; i++) {
            seq += 3;
        }
        System.out.println("inc long " + seq);
    }
}
