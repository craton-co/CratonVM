// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: TYPECHECK21-3, array targets inline
// (`CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS`, `ir_lower::typecheck_array_plan` /
// `typecheck_array_hit_bytes`, `Lowerer::emit_typecheck_array_arm`, the VM's
// `WELL_KNOWN_TYPECHECK_ARRAY_COMPONENT_TAG` answer).
//
// Risk targeted: the arm proves `x instanceof T[]` from the header's kind byte
// (reference array) and the component id. The dimension must not be lost: a
// `String[][]` is NOT a `String[]` but IS an `Object[]`; a primitive array is
// never an `Object[]`; `int[][]` IS `Object[]` and `Cloneable[]` and
// `Serializable[]`; a `Sub[]` is a `Base[]` and an `I1[]` when `Sub` implements
// `I1` through a superclass or a superinterface; an interface component
// (`I2[]` with `I2 extends I1`) IS an `I1[]`; null is never an instance.
// Every (value, target) pair is printed as a bit matrix, from a hot method
// per target (one instanceof site each), cold, hot, and after a deopt
// trigger (a component class loaded late whose arrays reach the hot sites).
//
// Run:
//   javac -d out R21FzTcArray1.java
//   java -Xmx256m -cp out R21FzTcArray1                                   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzTcArray1
//   CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS=1 cratonvm -Xmx256m -cp out R21FzTcArray1
//   CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS=1 CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM=1 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.io.Serializable;

public class R21FzTcArray1 {
    static final int WARM = 50_000;

    interface I1 {
    }

    interface I2 extends I1 {
    }

    interface I3 {
    }

    static class Base implements I3 {
    }

    static class Sub extends Base implements I2 {
    }

    static final class Leaf extends Sub {
    }

    static class LateComp extends Leaf0 {
    }

    static class Leaf0 extends Base implements I1 {
    }

    static boolean tObj(Object o) {
        return o instanceof Object[];
    }

    static boolean tObj2(Object o) {
        return o instanceof Object[][];
    }

    static boolean tBase(Object o) {
        return o instanceof Base[];
    }

    static boolean tSub(Object o) {
        return o instanceof Sub[];
    }

    static boolean tLeaf(Object o) {
        return o instanceof Leaf[];
    }

    static boolean tI1(Object o) {
        return o instanceof I1[];
    }

    static boolean tI3(Object o) {
        return o instanceof I3[];
    }

    static boolean tBase2(Object o) {
        return o instanceof Base[][];
    }

    static boolean tStr(Object o) {
        return o instanceof String[];
    }

    static boolean tCs(Object o) {
        return o instanceof CharSequence[];
    }

    static boolean tCl(Object o) {
        return o instanceof Cloneable[];
    }

    static boolean tSer(Object o) {
        return o instanceof Serializable[];
    }

    static boolean tInt(Object o) {
        return o instanceof int[];
    }

    static boolean tComparable(Object o) {
        return o instanceof Comparable[];
    }

    static int bits(Object o) {
        int b = 0;
        b = b << 1 | (tObj(o) ? 1 : 0);
        b = b << 1 | (tObj2(o) ? 1 : 0);
        b = b << 1 | (tBase(o) ? 1 : 0);
        b = b << 1 | (tSub(o) ? 1 : 0);
        b = b << 1 | (tLeaf(o) ? 1 : 0);
        b = b << 1 | (tI1(o) ? 1 : 0);
        b = b << 1 | (tI3(o) ? 1 : 0);
        b = b << 1 | (tBase2(o) ? 1 : 0);
        b = b << 1 | (tStr(o) ? 1 : 0);
        b = b << 1 | (tCs(o) ? 1 : 0);
        b = b << 1 | (tCl(o) ? 1 : 0);
        b = b << 1 | (tSer(o) ? 1 : 0);
        b = b << 1 | (tInt(o) ? 1 : 0);
        b = b << 1 | (tComparable(o) ? 1 : 0);
        return b;
    }

    static Object[] values() {
        return new Object[] {
            null, new Object(), "s", new Object[1], new Object[1][1], new Base[1], new Sub[1], new Leaf[1],
            new Base[1][1], new Sub[2][2], new I1[1], new I2[1], new I3[1], new String[1], new String[1][1],
            new CharSequence[1], new int[1], new int[1][1], new long[1][1][1], new Integer[1], new Comparable[1],
            new Cloneable[1], new Serializable[1], new Leaf0[1], new Base(), new Sub(), new Object[0][0][0],
        };
    }

    static String bin(int b) {
        String s = Integer.toBinaryString(b | (1 << 14));
        return s.substring(1);
    }

    static void rows(String tag, Object[] vs) {
        for (int i = 0; i < vs.length; i++) {
            Object v = vs[i];
            String name = v == null ? "null" : v.getClass().getName();
            System.out.println("row " + tag + " v" + i + " " + name + " " + bin(bits(v)));
        }
    }

    public static void main(String[] args) {
        Object[] vs = values();
        rows("cold", vs);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + bits(vs[r % vs.length]);
        }
        System.out.println("warm hash " + h);
        rows("hot", vs);
        Object[] late = {new LateComp[1], new LateComp[1][1], new LateComp[0]};
        rows("late", late);
        rows("after", vs);
    }
}
