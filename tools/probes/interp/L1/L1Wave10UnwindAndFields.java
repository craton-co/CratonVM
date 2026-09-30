// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Interpreter round i1 wave 10, lane L1: a no-debugger regression probe for
 * the two hooks this wave added to the interpreter's hottest shared paths —
 * the JDWP exception hook at the top of the unwinder (and its "caught"
 * twin at every handler entry), and the JDWP field-watch hook in the four
 * field bytecodes (read before the operands are popped). With no debugger
 * attached both are one load; this probe checks that the answers do not
 * move: exceptions caught, rethrown, wrapped, crossing a reflective call
 * (a second interpreter entry) and escaping a finally; every field type
 * read and written as instance and static, including long/double values
 * whose bit patterns look like tagged slots.
 *
 * HotSpot 25 prints (and CratonVM must print, with and without --nojit):
 *
 *   caught=1 rethrown=1 finally=2 wrapped=IllegalStateException/boom
 *   reflective=InvocationTargetException<-ArithmeticException
 *   loop=500 caughtInLoop=250
 *   fields=true statics=true
 *   longBits=-281474976710651 doubleBits=-281474976710651
 *   done
 *
 * Deterministic; no timing output.
 */
public class L1Wave10UnwindAndFields {
    static int si;
    static long sl;
    static double sd;
    static float sf;
    static byte sb;
    static char sc;
    static short ss;
    static boolean sz;
    static Object so;

    int i;
    long l;
    double d;
    float f;
    byte b;
    char c;
    short s;
    boolean z;
    Object o;

    static int finallyCount;

    static void thrower(int n) {
        if (n > 0) {
            throw new IllegalStateException("boom");
        }
    }

    static int caughtOnce() {
        try {
            thrower(1);
            return 0;
        } catch (IllegalStateException e) {
            return 1;
        }
    }

    static int rethrown() {
        try {
            try {
                thrower(1);
            } catch (IllegalStateException e) {
                throw e;
            } finally {
                finallyCount++;
            }
        } catch (IllegalStateException e) {
            return 1;
        } finally {
            finallyCount++;
        }
        return 0;
    }

    static String wrapped() {
        try {
            try {
                thrower(1);
            } catch (IllegalStateException e) {
                throw new RuntimeException(e);
            }
        } catch (RuntimeException e) {
            Throwable c = e.getCause();
            return c.getClass().getSimpleName() + "/" + c.getMessage();
        }
        return "none";
    }

    public static int divide(int a, int b) {
        return a / b;
    }

    static String reflective() throws Exception {
        java.lang.reflect.Method m =
                L1Wave10UnwindAndFields.class.getMethod("divide", int.class, int.class);
        try {
            m.invoke(null, 1, 0);
            return "none";
        } catch (java.lang.reflect.InvocationTargetException e) {
            return e.getClass().getSimpleName() + "<-" + e.getCause().getClass().getSimpleName();
        }
    }

    public static void main(String[] args) throws Exception {
        int caught = caughtOnce();
        int re = rethrown();
        System.out.println("caught=" + caught + " rethrown=" + re + " finally=" + finallyCount
                + " wrapped=" + wrapped());
        System.out.println("reflective=" + reflective());

        int loop = 0;
        int caughtInLoop = 0;
        for (int k = 0; k < 500; k++) {
            try {
                thrower(k & 1);
                loop++;
            } catch (IllegalStateException e) {
                caughtInLoop++;
                loop++;
            }
        }
        System.out.println("loop=" + loop + " caughtInLoop=" + caughtInLoop);

        L1Wave10UnwindAndFields p = new L1Wave10UnwindAndFields();
        long bits = 0xFFFF_0000_0000_0005L;
        for (int k = 0; k < 1000; k++) {
            p.i = k;
            p.l = bits;
            p.d = Double.longBitsToDouble(bits);
            p.f = k * 0.5f;
            p.b = (byte) k;
            p.c = (char) ('a' + (k % 26));
            p.s = (short) (k * 7);
            p.z = (k & 1) == 0;
            p.o = (k % 3 == 0) ? null : p;
            si = p.i;
            sl = p.l;
            sd = p.d;
            sf = p.f;
            sb = p.b;
            sc = p.c;
            ss = p.s;
            sz = p.z;
            so = p.o;
        }
        boolean fields = p.i == 999 && p.l == bits && p.f == 499.5f && p.b == (byte) 999
                && p.c == (char) ('a' + (999 % 26)) && p.s == (short) (999 * 7) && !p.z
                && p.o == null;
        boolean statics = si == 999 && sl == bits && sf == 499.5f && sb == (byte) 999
                && sc == p.c && ss == p.s && !sz && so == null;
        System.out.println("fields=" + fields + " statics=" + statics);
        System.out.println("longBits=" + sl + " doubleBits=" + Double.doubleToRawLongBits(sd));
        System.out.println("done");
    }
}
