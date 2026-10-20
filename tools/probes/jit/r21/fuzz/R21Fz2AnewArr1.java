// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: ALLOC21-5, the optimizing tier's inline
// `anewarray` bump on Generational and G1
// (`CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC`;
// `runtime_lowering::emit_inline_tlab_newarray_ir_impl`'s `ref_admitted`,
// `ir_lower`'s `ref_inline`). It was ZGC-only (an announce); on the other
// collectors every compiled `new T[n]` called `jit_anewarray_object`.
//
// Risk targeted: the inline arm writes the header (class word = the
// COMPONENT class id, the array kind, the length) and relies on the zeroed
// TLAB chunk for an all-null body. Wrong header -> wrong getClass(), a store
// check that passes or fails wrongly (ArrayStoreException through an Object[]
// view), a wrong length; a non-zero body -> garbage references the next GC
// chases. A young array is then filled with references to OLD objects and to
// young ones across collections (card / remembered-set work). Lengths 0, 1,
// a variable n, a TLAB-busting 1<<16 (the slow path), component classes:
// Object, String, an interface, a nested array type (`new int[n][]`), an
// enum. Cold, hot (200k trips), System.gc() mid-loop with the arrays held,
// a late component class (the deopt trigger), again.
//
// Run:
//   javac -d out R21Fz2AnewArr1.java
//   java -Xmx512m -cp out R21Fz2AnewArr1                                 # HotSpot 25
//   cratonvm -Xmx512m -cp out R21Fz2AnewArr1                              (default collector)
//   CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC=1 cratonvm -Xmx512m -cp out R21Fz2AnewArr1
//   CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC=1 cratonvm -Xmx512m -XX:+UseG1GC -cp out R21Fz2AnewArr1
//   CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC=1 cratonvm -Xmx512m -XX:+UseZGC -cp out R21Fz2AnewArr1   (no change expected)
//   CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC=1 cratonvm -Xmx64m ... (more collections)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.ArrayList;
import java.util.Arrays;

public class R21Fz2AnewArr1 {
    static final int WARM = 200_000;

    interface Shape {
        int area();
    }

    static final class Sq implements Shape {
        final int s;

        Sq(int s) {
            this.s = s;
        }

        public int area() {
            return s * s;
        }
    }

    enum Color { RED, GREEN }

    static final Object[] OLD = new Object[64];

    static Object[] objs(int n) {
        return new Object[n];
    }

    static String[] strs(int n) {
        return new String[n];
    }

    static Shape[] shapes(int n) {
        return new Shape[n];
    }

    static int[][] rows(int n) {
        return new int[n][];
    }

    static Color[] colors(int n) {
        return new Color[n];
    }

    static Object[] fixed() {
        return new Object[16];
    }

    static String nulls(Object[] a) {
        int nonNull = 0;
        for (Object o : a) {
            if (o != null) {
                nonNull++;
            }
        }
        return a.getClass().getSimpleName() + "/" + a.length + "/" + nonNull;
    }

    static String store(Object[] a, Object v) {
        if (a.length == 0) {
            return "empty";
        }
        try {
            a[a.length - 1] = v;
            return "ok";
        } catch (ArrayStoreException e) {
            return "ASE(" + e.getMessage() + ")";
        }
    }

    static long fill(int n) {
        long s = 0;
        Shape[] sh = shapes(n);
        Object[] o = objs(n);
        String[] st = strs(n);
        for (int i = 0; i < n; i++) {
            sh[i] = new Sq(i);
            o[i] = OLD[i & 63];
            st[i] = (i & 1) == 0 ? "e" : null;
        }
        for (int i = 0; i < n; i++) {
            s += sh[i].area() + (o[i] == OLD[i & 63] ? 1 : 0) + (st[i] == null ? 0 : 2);
        }
        return s + rows(n).length + colors(n).length + fixed().length;
    }

    static void report(String tag) {
        int[] ns = {0, 1, 7, 64, 1 << 16};
        for (int n : ns) {
            Object[] o = objs(n);
            System.out.println("row " + tag + " n=" + n + " | " + nulls(o) + " " + nulls(strs(n)) + " "
                    + nulls(shapes(n)) + " " + nulls(rows(n)) + " " + nulls(colors(n)) + " | store "
                    + store(o, "s") + " " + store(strs(n), "s") + " " + store(strs(n), Integer.valueOf(1)) + " "
                    + store(shapes(n), new Sq(2)) + " " + store(shapes(n), "x") + " " + store(rows(n), new int[1])
                    + " " + store(rows(n), new long[1]) + " " + store(colors(n), Color.GREEN) + " | fill "
                    + (n <= 64 ? fill(n) : -1));
        }
        System.out.println("row " + tag + " fixed " + nulls(fixed()) + " " + Arrays.toString(fixed()).length()
                + " clone " + nulls(fixed().clone()) + " comp " + fixed().getClass().getComponentType().getName()
                + " " + shapes(1).getClass().getComponentType().getName() + " " + rows(1).getClass().getName());
    }

    public static void main(String[] args) {
        for (int i = 0; i < OLD.length; i++) {
            OLD[i] = "old" + i;
        }
        System.gc(); // OLD and its strings are now old
        report("cold");
        long h = 0;
        ArrayList<Object[]> held = new ArrayList<>();
        for (int r = 0; r < WARM; r++) {
            int n = r & 15;
            h = h * 31 + fill(n);
            if ((r & 1023) == 0) {
                Object[] keep = objs(8);
                for (int i = 0; i < 8; i++) {
                    keep[i] = (i & 1) == 0 ? OLD[i] : new Sq(r + i);
                }
                held.add(keep);
            }
            if (r == WARM / 2) {
                System.gc();
            }
        }
        System.out.println("warm hash " + h);
        System.gc();
        long check = 0;
        for (Object[] keep : held) {
            for (int i = 0; i < 8; i++) {
                Object v = keep[i];
                check = check * 31 + ((i & 1) == 0 ? (v == OLD[i] ? 1 : 0) : ((Sq) v).area() & 0xFFFF);
            }
        }
        System.out.println("held " + held.size() + " check " + check);
        report("hot");
        // Deopt trigger: a component class first seen now, through the same
        // shapes() site (an anonymous Shape) and a new array component.
        Shape late = new Shape() {
            public int area() {
                return -1;
            }
        };
        Shape[] ls = shapes(3);
        ls[0] = late;
        System.out.println("late " + ls[0].area() + " " + nulls(ls) + " " + store(ls, late));
        Object[] lateArr = new Thread.State[5];
        System.out.println("late2 " + nulls(lateArr) + " " + store(lateArr, Thread.State.NEW) + " " + store(lateArr, "x"));
        report("after");
    }
}
