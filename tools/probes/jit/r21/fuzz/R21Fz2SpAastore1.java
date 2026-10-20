// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: TYPECHECK21-5, the single-pass `aastore`
// display arm (`CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM`, needs the default-ON
// `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM`; `x64/op_array.rs` aastore:
// `MOV EDX, R10D; MOV ECX, [R11 + class_id_off]` in front of
// `ir_lower::aastore_display_arm_bytes`).
//
// Risk targeted: a false YES stores an illegal element (heap pollution: a
// later read through the array's static type hands out the wrong class); a
// register the arm clobbers (RCX, RDX, R10) that still held the array, the
// index or the value would store into the wrong slot or array. Each row
// stores through an `Object[]` view into an array whose RUNTIME component is
// the target: a superclass, an interface implemented by a superclass, a
// superinterface of a superinterface, a generic interface, `Object[]`
// (component id 0), `Cloneable[]` / `Serializable[]` / `Object[][]` /
// `Number[]` / `Comparable[]` receiving arrays, boxes and strings, enum
// constants, lambdas (their interfaces are by name, not by id) and nulls.
// After every store the probe reads the slot back (identity) and its
// neighbours (untouched), and prints the ASE text for each refusal. Cold,
// hot (50k calls per site), a late class (the deopt / new-class trigger), again.
//
// Run:
//   javac -d out R21Fz2SpAastore1.java
//   java -Xmx256m -cp out R21Fz2SpAastore1                               # HotSpot 25
//   CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM=1 CRATONVM_C2_ACCEPT=never cratonvm -Xmx256m -cp out R21Fz2SpAastore1
//   CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM=1 cratonvm -Xmx256m -cp out R21Fz2SpAastore1
//   CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM=1 CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM=1 cratonvm ...
//   CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM=1 CRATONVM_JIT_TYPECHECK_DISPLAY_ARM=0 cratonvm ... (arm inert: control)
//   cratonvm -Xmx256m -cp out R21Fz2SpAastore1, cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.io.Serializable;
import java.lang.reflect.Array;
import java.util.function.Supplier;

public class R21Fz2SpAastore1 {
    static final int WARM = 50_000;

    interface Root {
    }

    interface Mid extends Root {
    }

    interface Leaf extends Mid {
    }

    interface Gen<T> {
    }

    static class Base implements Leaf {
    }

    static class Sub extends Base implements Gen<String> {
    }

    static class Other {
    }

    static class Late extends Sub {
    }

    enum Ev implements Mid { X, Y }

    // One aastore site per call shape: the site's profile stays its own.
    static String put(Object[] arr, int i, Object v) {
        Object before = arr[(i + 1) % arr.length];
        try {
            arr[i] = v;
        } catch (ArrayStoreException e) {
            // A lambda's class name carries a run-specific address suffix.
            String m = String.valueOf(e.getMessage());
            int k = m.indexOf("$$Lambda");
            return "ASE(" + (k >= 0 ? m.substring(0, k) + "$$Lambda" : m) + ")";
        }
        boolean same = arr[i] == v;
        boolean neighbour = arr[(i + 1) % arr.length] == before;
        return same && neighbour ? "ok" : "BAD(" + same + "," + neighbour + ")";
    }

    static Object[][] arrays() {
        return new Object[][] {
            new Object[3], new Root[3], new Mid[3], new Leaf[3], new Gen<?>[3], new Base[3], new Sub[3],
            new Cloneable[3], new Serializable[3], new Object[3][], new Number[3], new Comparable<?>[3],
            new Ev[3], new Runnable[3], new Supplier<?>[3], new int[3][], new String[3],
            // A hidden (lambda) class as the component: its id must not read as
            // "unnamed, so Object" (`array_descriptor_of`'s fallback).
            (Object[]) Array.newInstance(LAMBDA.getClass(), 3),
        };
    }

    static Object[] values() {
        Runnable lambda = LAMBDA;
        Supplier<String> sup = () -> "s";
        return new Object[] {
            null, new Object(), new Base(), new Sub(), new Other(), Ev.Y, "str", Integer.valueOf(4), 2.5,
            new int[1], new Object[1], new String[1], new int[1][1], lambda, sup, new Sub[0], new Leaf[0],
        };
    }

    static final Runnable LAMBDA = () -> { };

    static String name(Object[] a) {
        String n = a.getClass().getComponentType().getSimpleName();
        int k = n.indexOf("$$Lambda");
        return k >= 0 ? n.substring(0, k) + "$$Lambda" : n;
    }

    static void rows(String tag, Object[][] arrs, Object[] vals) {
        for (Object[] arr : arrs) {
            StringBuilder sb = new StringBuilder("row " + tag + " " + name(arr));
            for (int v = 0; v < vals.length; v++) {
                sb.append(" | ").append(put(arr, v % 3, vals[v]));
            }
            System.out.println(sb);
        }
    }

    public static void main(String[] args) {
        Object[][] arrs = arrays();
        Object[] vals = values();
        rows("cold", arrs, vals);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            Object[] arr = arrs[r % arrs.length];
            for (int v = 0; v < vals.length; v++) {
                h = h * 31 + put(arr, (r + v) % 3, vals[v]).length();
            }
        }
        System.out.println("warm hash " + h);
        rows("hot", arrays(), vals);
        Object[] late = {new Late(), new Late[1], new Ev[0], Ev.X};
        rows("late", arrays(), late);
        rows("after", arrays(), vals);
    }
}
