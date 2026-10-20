// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the IR `aastore` display arm
// (`CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM`, `ir_lower::aastore_display_arm_bytes`).
//
// Risk targeted: the arm proves a store legal from the RUN-TIME component id
// `c` (read from the array) and the value's class `v`: `c`'s class display at
// `c`'s depth, else `c` in `v`'s transitive interface set. A false YES lets a
// covariant store through (heap pollution, no ArrayStoreException). Rows:
//   * `Object[] a = new String[1]; a[0] = Integer` (must throw ASE);
//   * `Base[]` holding a `Sub[]`, storing a `Base` (ASE) and a `Sub2` (ASE)
//     and a `SubSub` (ok);
//   * interface components: `I1[]` with `I2 extends I1` values, a class that
//     implements I1 only through its SUPERCLASS, and a class that implements
//     an unrelated interface (ASE);
//   * `Cloneable[]` / `Serializable[]` / `Object[][]` arrays receiving arrays
//     (int[] is Cloneable and Serializable; String[] is not an Object[][]
//     element... it IS: `Object[][]`'s component is `Object[]`);
//   * a component class whose subclass is loaded late (the deopt trigger);
//   * null stores (always legal).
// One aastore site per kernel, the array's dynamic type varying under one
// static type, the ASE message printed (HotSpot: the stored value's class
// name).
//
// Run:
//   javac -d out R21FzTcAastore1.java
//   java -Xmx256m -cp out R21FzTcAastore1                                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzTcAastore1
//   CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM=1 cratonvm -Xmx256m -cp out R21FzTcAastore1
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.io.Serializable;

public class R21FzTcAastore1 {
    static final int WARM = 40_000;

    interface I1 {
    }

    interface I2 extends I1 {
    }

    interface Other {
    }

    static class Base {
    }

    static class Sub extends Base implements I2 {
    }

    static class SubSub extends Sub {
    }

    static class Sub2 extends Base {
    }

    static class ViaSuper extends Sub {
    }

    static class Unrelated implements Other {
    }

    static class LateSub extends SubSub {
    }

    static int store(Object[] a, Object v) {
        a[0] = v;
        return 1;
    }

    static String one(Object[] a, Object v) {
        try {
            store(a, v);
            Object back = a[0];
            return back == null ? "ok:null" : "ok";
        } catch (ArrayStoreException e) {
            return "ASE(" + e.getMessage() + ")";
        }
    }

    static Object[][] arrays() {
        return new Object[][] {
            new Object[1], new String[1], new Base[1], new Sub[1], new SubSub[1], new I1[1], new I2[1],
            new Other[1], new Cloneable[1], new Serializable[1], new Object[1][], new Base[1][], new Comparable[1],
            new CharSequence[1],
        };
    }

    static Object[] values() {
        return new Object[] {
            null, "s", 7, new Base(), new Sub(), new SubSub(), new Sub2(), new ViaSuper(), new Unrelated(),
            new int[1], new String[1], new Base[1], new Sub[1], new Object(), new StringBuilder("sb"),
        };
    }

    static void rows(String tag, Object[] vs) {
        Object[][] as = arrays();
        for (int i = 0; i < as.length; i++) {
            StringBuilder sb = new StringBuilder("row " + tag + " " + as[i].getClass().getName());
            for (Object v : vs) {
                sb.append(' ').append(one(as[i], v));
            }
            System.out.println(sb);
        }
    }

    public static void main(String[] args) {
        Object[] vs = values();
        rows("cold", vs);
        Object[][] as = arrays();
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            Object[] a = as[r % as.length];
            Object v = vs[(r / as.length) % vs.length];
            h = h * 31 + one(a, v).length();
        }
        System.out.println("warm hash " + h);
        rows("hot", vs);
        Object[] late = {new LateSub(), new LateSub[1]};
        rows("late", late);
        rows("after", vs);
    }
}
