// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: TYPECHECK21-6, a `checkcast` dominated by
// the same `instanceof` (`CRATONVM_JIT_IR_CHECKCAST_AFTER_INSTANCEOF`,
// `ir::checkcast_proven_by_instanceof_edge`).
//
// Risk targeted: the fold matches `If(Cmp(Eq|Ne, InstanceOf(obj, site), 0))`
// and the builder's control token. A cast that is NOT dominated by a hit of
// the same value and site must keep its check:
//   * the negated test (`if (!(o instanceof T)) return (T) o;` must throw);
//   * the variable reassigned inside the branch (`o = other; (T) o`);
//   * the instanceof result stored in a boolean, the variable changed, then
//     tested (`boolean b = o instanceof T; o = x; if (b) (T) o`);
//   * two different targets (`o instanceof A` then `(B) o`);
//   * a cast after the if/else JOIN (control token replaced at the join);
//   * a cast in a loop after an instanceof of the previous element;
//   * a `&&` chain and a ternary;
//   * switch patterns with guards, record patterns.
// Each row prints the result or the CCE message. Cold, hot, after a deopt
// trigger (a late subclass that reaches the same sites).
//
// Run:
//   javac -d out R21FzTcPattern1.java
//   java -Xmx256m -cp out R21FzTcPattern1                                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzTcPattern1
//   CRATONVM_JIT_IR_CHECKCAST_AFTER_INSTANCEOF=1 cratonvm -Xmx256m -cp out R21FzTcPattern1
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzTcPattern1 {
    static final int WARM = 60_000;

    static class A {
        int v = 1;
    }

    static class B extends A {
        int w = 2;
    }

    static class C {
        int z = 3;
    }

    static final class LateB extends B {
        LateB() {
            w = 20;
        }
    }

    record P(Object x, int y) {
    }

    static int pattern(Object o) {
        if (o instanceof B b) {
            return b.w;
        }
        return -1;
    }

    static int negated(Object o) {
        if (!(o instanceof B)) {
            return ((B) o).w; // reached only for non-B: must throw (or NPE for null? no: null casts fine)
        }
        return 0;
    }

    static int reassigned(Object o, Object other) {
        if (o instanceof B) {
            o = other;
            return ((B) o).w;
        }
        return -2;
    }

    static int savedFlag(Object o, Object other) {
        boolean isB = o instanceof B;
        o = other;
        if (isB) {
            return ((B) o).w;
        }
        return -3;
    }

    static int twoTargets(Object o) {
        if (o instanceof A) {
            return ((B) o).w;
        }
        return -4;
    }

    static int afterJoin(Object o, boolean flip) {
        int t;
        if (o instanceof B) {
            t = 1;
        } else {
            t = 2;
        }
        if (flip) {
            t += 10;
        }
        return t + ((B) o).w;
    }

    static int loop(Object[] os) {
        int s = 0;
        Object prev = null;
        for (Object o : os) {
            if (prev instanceof B) {
                s += ((B) o).w;
            }
            prev = o;
        }
        return s;
    }

    static int chain(Object o, Object p) {
        if (o instanceof A && p instanceof B) {
            return ((B) p).w + ((A) o).v;
        }
        return o instanceof C ? ((C) o).z : -5;
    }

    static int sw(Object o) {
        return switch (o) {
            case B b when b.w > 5 -> 100 + b.w;
            case B b -> b.w;
            case A a -> a.v;
            case P(B b, int y) -> b.w * 1000 + y;
            case P(Object x, int y) -> y;
            case null -> -7;
            default -> -6;
        };
    }

    static String run(java.util.function.IntSupplier s) {
        try {
            return Integer.toString(s.getAsInt());
        } catch (ClassCastException e) {
            return "CCE(" + e.getMessage() + ")";
        } catch (NullPointerException e) {
            return "NPE";
        }
    }

    static void rows(String tag, Object[] vs) {
        for (int i = 0; i < vs.length; i++) {
            Object o = vs[i];
            Object other = vs[(i + 1) % vs.length];
            boolean flip = (i & 1) == 0;
            StringBuilder sb = new StringBuilder("row " + tag + " v" + i);
            sb.append(' ').append(run(() -> pattern(o)));
            sb.append(' ').append(run(() -> negated(o)));
            sb.append(' ').append(run(() -> reassigned(o, other)));
            sb.append(' ').append(run(() -> savedFlag(o, other)));
            sb.append(' ').append(run(() -> twoTargets(o)));
            sb.append(' ').append(run(() -> afterJoin(o, flip)));
            sb.append(' ').append(run(() -> chain(o, other)));
            sb.append(' ').append(run(() -> sw(o)));
            System.out.println(sb);
        }
        System.out.println("row " + tag + " loop " + run(() -> loop(vs)));
        Object[] goodLoop = {new B(), new B(), new B(), new A()};
        System.out.println("row " + tag + " loop-good " + run(() -> loop(goodLoop)));
    }

    static Object[] values() {
        return new Object[] {new B(), new A(), new C(), null, "s", new P(new B(), 4), new P("x", 5), new B()};
    }

    public static void main(String[] args) {
        Object[] vs = values();
        rows("cold", vs);
        long h = 0;
        Object b = new B();
        Object a = new A();
        Object[] arr = {b, b, b, b};
        for (int r = 0; r < WARM; r++) {
            Object o = (r & 3) == 0 ? a : b;
            h = h * 31 + pattern(o);
            h = h * 31 + negated(b);
            h = h * 31 + reassigned(o, b);
            h = h * 31 + savedFlag(o, b);
            h = h * 31 + twoTargets(b);
            h = h * 31 + afterJoin(b, (r & 1) == 0);
            h = h * 31 + loop(arr);
            h = h * 31 + chain(o, b);
            h = h * 31 + sw((r & 7) == 0 ? new P(b, r) : o);
        }
        System.out.println("warm hash " + h);
        rows("hot", vs);
        Object[] late = {new LateB(), new P(new LateB(), 9), new A()};
        rows("late", late);
        rows("after", vs);
    }
}
