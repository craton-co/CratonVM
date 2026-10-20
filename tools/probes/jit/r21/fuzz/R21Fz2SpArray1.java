// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: TYPECHECK21-3b, the single-pass
// `instanceof` / `checkcast` array arm (`CRATONVM_JIT_SP_TYPECHECK_ARRAY_TARGETS`;
// `x64/op_object.rs` `Compiler::sp_emit_typecheck_array_arm`, the IR's
// `typecheck_array_hit_bytes` / `typecheck_array_plan_any_tier`).
//
// Risk targeted: the arm answers an ARRAY receiver of a one-dimensional
// `[LT;` target from the receiver's component (a hit), sending anything it
// cannot prove to the helper. A false hit is wrong `instanceof` / a cast
// that should throw. The matrix crosses targets `Object[]`, `Number[]`,
// `Comparable[]`, `CharSequence[]`, `Cloneable[]`, `Serializable[]`, `Root[]`
// (an interface), `Base[]`, `Runnable[]` against receivers whose component
// is a primitive ARRAY (`int[][]` IS an `Object[]`, a `Cloneable[]` and a
// `Serializable[]`, never a `Number[]`), a primitive (`int[]` is none), an
// interface, a subclass, an enum, a box, a String, multi-dimensional
// reference arrays, lambdas' arrays (`Runnable[]` holding a lambda), and
// non-arrays (the plain arms after the arm). Each checkcast result is USED
// (length + component name) so a false hit surfaces. CCE texts printed.
// Cold, hot (50k calls per site), a late component class, again.
//
// Run:
//   javac -d out R21Fz2SpArray1.java
//   java -Xmx256m -cp out R21Fz2SpArray1                                 # HotSpot 25
//   CRATONVM_JIT_SP_TYPECHECK_ARRAY_TARGETS=1 CRATONVM_C2_ACCEPT=never cratonvm -Xmx256m -cp out R21Fz2SpArray1
//   CRATONVM_JIT_SP_TYPECHECK_ARRAY_TARGETS=1 CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS=1 cratonvm -Xmx256m -cp out R21Fz2SpArray1
//   ... + CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM=1 CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY=1
//   cratonvm -Xmx256m -cp out R21Fz2SpArray1, cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.io.Serializable;

public class R21Fz2SpArray1 {
    static final int WARM = 50_000;

    interface Root {
    }

    static class Base implements Root {
    }

    static class Sub extends Base {
    }

    static class Late extends Sub {
    }

    enum En { A }

    // instanceof, one site per target.
    static int i0(Object o) { return o instanceof Object[] ? 1 : 0; }
    static int i1(Object o) { return o instanceof Number[] ? 1 : 0; }
    static int i2(Object o) { return o instanceof Comparable<?>[] ? 1 : 0; }
    static int i3(Object o) { return o instanceof CharSequence[] ? 1 : 0; }
    static int i4(Object o) { return o instanceof Cloneable[] ? 1 : 0; }
    static int i5(Object o) { return o instanceof Serializable[] ? 1 : 0; }
    static int i6(Object o) { return o instanceof Root[] ? 1 : 0; }
    static int i7(Object o) { return o instanceof Base[] ? 1 : 0; }
    static int i8(Object o) { return o instanceof Runnable[] ? 1 : 0; }

    // checkcast, one site per target, result used.
    static String use(Object[] a) {
        return a.length + a.getClass().getComponentType().getSimpleName();
    }

    static String c0(Object o) { return use((Object[]) o); }
    static String c1(Object o) { return use((Number[]) o); }
    static String c2(Object o) { return use((Comparable<?>[]) o); }
    static String c3(Object o) { return use((CharSequence[]) o); }
    static String c4(Object o) { return use((Cloneable[]) o); }
    static String c5(Object o) { return use((Serializable[]) o); }
    static String c6(Object o) { return use((Root[]) o); }
    static String c7(Object o) { return use((Base[]) o); }
    static String c8(Object o) { return use((Runnable[]) o); }

    static String cast(int k, Object o) {
        try {
            switch (k) {
                case 0: return c0(o);
                case 1: return c1(o);
                case 2: return c2(o);
                case 3: return c3(o);
                case 4: return c4(o);
                case 5: return c5(o);
                case 6: return c6(o);
                case 7: return c7(o);
                default: return c8(o);
            }
        } catch (ClassCastException e) {
            return "CCE(" + e.getMessage() + ")";
        } catch (NullPointerException e) {
            return "NPE";
        }
    }

    static int inst(Object o) {
        return i0(o) | i1(o) << 1 | i2(o) << 2 | i3(o) << 3 | i4(o) << 4 | i5(o) << 5 | i6(o) << 6 | i7(o) << 7
                | i8(o) << 8;
    }

    static Object[] values() {
        Runnable[] lam = {() -> { }};
        return new Object[] {
            null, new int[2][3], new long[1][], new int[1], new Object[1], new Object[1][1], new Integer[2],
            new String[2], new StringBuilder[1], new Base[1], new Sub[2], new Root[1], new En[1], new Base[1][1],
            new Cloneable[1], new Serializable[0], lam, new Thread[1], "plain", new Object(), new int[0][0][0],
        };
    }

    static void rows(String tag, Object[] vs) {
        for (int i = 0; i < vs.length; i++) {
            StringBuilder sb = new StringBuilder("row " + tag + " v" + i + " inst=" + inst(vs[i]));
            for (int k = 0; k < 9; k++) {
                sb.append(" | ").append(cast(k, vs[i]));
            }
            System.out.println(sb);
        }
    }

    public static void main(String[] args) {
        Object[] vs = values();
        rows("cold", vs);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            Object o = vs[r % vs.length];
            h = h * 31 + inst(o);
            for (int k = 0; k < 9; k++) {
                h = h * 31 + cast(k, o).length();
            }
        }
        System.out.println("warm hash " + h);
        rows("hot", vs);
        Object[] late = {new Late[1], new Late[1][1], new En[0][0], new Runnable[0][0]};
        rows("late", late);
        rows("after", vs);
    }
}
