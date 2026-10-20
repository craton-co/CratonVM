// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the single-pass `instanceof` display arm
// (`CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM`) and `jit_checkcast` publishing
// the receiver's display (`CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY`).
//
// Risk targeted: a display MISS answered inline is only right when the
// receiver's interface set is TRANSITIVELY complete (superinterfaces of
// superinterfaces, interfaces of every superclass) and the verdict says the
// hierarchy is final. Shapes: a diamond (`I4 extends I2, I3`, both extend
// `I1`), an interface reached only through an abstract superclass's
// superinterface, a generic interface, a class whose superclass is loaded
// lazily, and receivers whose class was first used BEFORE its `<clinit>`
// completed (a cast and an instanceof executed inside the class's own
// `<clinit>`, the display-publication gap TYPECHECK21-4 targets). A bit
// matrix per receiver, one instanceof / checkcast site per target, cold,
// hot, after a deopt trigger (a late class implementing the deepest
// interface).
//
// Run:
//   javac -d out R21FzTcIface1.java
//   java -Xmx256m -cp out R21FzTcIface1                                   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzTcIface1
//   CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM=1 CRATONVM_C2_ACCEPT=never cratonvm -Xmx256m -cp out R21FzTcIface1
//   CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM=1 cratonvm ...
//   CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY=1 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzTcIface1 {
    static final int WARM = 60_000;

    interface I1 {
    }

    interface I2 extends I1 {
    }

    interface I3 extends I1 {
    }

    interface I4 extends I2, I3 {
    }

    interface G<T> {
    }

    interface Lone {
    }

    abstract static class Abs implements I3 {
    }

    static class C1 extends Abs {
    }

    static class C2 implements I4 {
    }

    static class C3 extends C2 implements G<String> {
    }

    static class C4 implements Lone, G<Integer> {
    }

    static final class C5 extends C1 implements I2 {
    }

    /** Casts and tests ITSELF inside its <clinit>, before it is initialized. */
    static class SelfInit implements I2 {
        static final String SEEN;

        static {
            Object me = new SelfInit();
            I2 cast = (I2) me;
            SEEN = (me instanceof I1) + "," + (me instanceof I3) + "," + (cast instanceof I4) + "," + early(me);
        }
    }

    static String early(Object o) {
        return bitsOf(o) + "/" + castBits(o);
    }

    static class LateDeep implements I4, Lone {
    }

    static boolean t1(Object o) {
        return o instanceof I1;
    }

    static boolean t2(Object o) {
        return o instanceof I2;
    }

    static boolean t3(Object o) {
        return o instanceof I3;
    }

    static boolean t4(Object o) {
        return o instanceof I4;
    }

    static boolean tg(Object o) {
        return o instanceof G<?>;
    }

    static boolean tl(Object o) {
        return o instanceof Lone;
    }

    static boolean ta(Object o) {
        return o instanceof Abs;
    }

    static boolean tc2(Object o) {
        return o instanceof C2;
    }

    static int bitsOf(Object o) {
        int b = 0;
        b = b << 1 | (t1(o) ? 1 : 0);
        b = b << 1 | (t2(o) ? 1 : 0);
        b = b << 1 | (t3(o) ? 1 : 0);
        b = b << 1 | (t4(o) ? 1 : 0);
        b = b << 1 | (tg(o) ? 1 : 0);
        b = b << 1 | (tl(o) ? 1 : 0);
        b = b << 1 | (ta(o) ? 1 : 0);
        b = b << 1 | (tc2(o) ? 1 : 0);
        return b;
    }

    static int c1(Object o) {
        try {
            I1 x = (I1) o;
            return x == null ? 2 : 1;
        } catch (ClassCastException e) {
            return 0;
        }
    }

    static int c4(Object o) {
        try {
            I4 x = (I4) o;
            return x == null ? 2 : 1;
        } catch (ClassCastException e) {
            return 0;
        }
    }

    static int cl(Object o) {
        try {
            Lone x = (Lone) o;
            return x == null ? 2 : 1;
        } catch (ClassCastException e) {
            return 0;
        }
    }

    static int castBits(Object o) {
        return c1(o) * 9 + c4(o) * 3 + cl(o);
    }

    static Object[] values() {
        return new Object[] {null, new Object(), new C1(), new C2(), new C3(), new C4(), new C5(), "s",
            new Object[0]};
    }

    static void rows(String tag, Object[] vs) {
        StringBuilder sb = new StringBuilder("row " + tag);
        for (Object v : vs) {
            sb.append(' ').append(Integer.toBinaryString(bitsOf(v) | 256).substring(1)).append(':')
                    .append(castBits(v));
        }
        System.out.println(sb);
    }

    public static void main(String[] args) {
        Object[] vs = values();
        rows("cold", vs);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            Object v = vs[r % vs.length];
            h = h * 31 + bitsOf(v);
            h = h * 31 + castBits(v);
        }
        System.out.println("warm hash " + h);
        rows("hot", vs);
        System.out.println("selfinit " + SelfInit.SEEN);
        Object[] more = {new SelfInit(), new LateDeep()};
        rows("late", more);
        rows("after", vs);
    }
}
