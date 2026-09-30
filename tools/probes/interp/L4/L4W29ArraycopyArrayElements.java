// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29, lane L4: `System.arraycopy` between
// reference arrays (`native-builtins/src/lang_system.rs`
// `native_system_arraycopy`, which the interpreter's arraycopy intrinsic and
// the JIT's reference-array bail-out both reach, in every mode).
//
//   h01-h05  an ARRAY element copied into a reference array whose component
//            is that element array's own component (or a supertype of it).
//            A reference array's heap header holds its COMPONENT class id, so
//            the per-element check read a `String[]` element as a `String`
//            and STORED it: `String[]` slots holding `String[]`s, and h05's
//            `String.length()` ran on an array. The element check now asks
//            the `aastore` predicate first for an array element.
//   h06-h09  array elements that are assignable (still copied).
//   t01-t11  HotSpot's two sentences (`ObjArrayKlass::do_copy`): "type
//            mismatch: can not copy S[] into D[]" when the destination
//            component is not a subtype of the source's, "element type
//            mismatch" otherwise; components print as `Klass::external_name()`
//            (`[Ljava.lang.Object;[]` for an `Object[][]` source). CratonVM
//            said "element type mismatch" for every refusal and rendered an
//            array source as `java.lang.Object[][]`. t02 / t07 / t08 show
//            the prefix before the refused element committed; t10 / t11 are
//            `Arrays.copyOf`, which shares the message.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W29ArraycopyArrayElements
// (the same lines in every mode)
//
// HotSpot 25 (25.0.3, default and -Xint) prints exactly:
//   h01 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, java.lang.String
//   h02 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, java.lang.Number
//   h03 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, java.lang.Comparable
//   h04 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, [Ljava.lang.String;
//   h05 java.lang.ArrayStoreException
//   h06 copied; dst[0] is [Ljava.lang.String;
//   h07 copied; dst[0] is [Ljava.lang.String;
//   h08 copied; dst[0] is [I
//   h09 copied; dst[0] is [[Ljava.lang.String;
//   t01 java.lang.ArrayStoreException: arraycopy: type mismatch: can not copy java.lang.String[] into java.lang.Integer[]
//   t02 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, java.lang.Integer
//   t02d [1, null, null]
//   t03 java.lang.ArrayStoreException: arraycopy: type mismatch: can not copy [Ljava.lang.String;[] into [Ljava.lang.Integer;[]
//   t04 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of [Ljava.lang.Object;[] to the type of the destination array, [Ljava.lang.String;
//   t05 java.lang.ArrayStoreException: arraycopy: type mismatch: can not copy [I[] into [J[]
//   t06 java.lang.ArrayStoreException: arraycopy: type mismatch: can not copy java.lang.Number[] into java.lang.String[]
//   t07 java.lang.ArrayStoreException: arraycopy: type mismatch: can not copy java.lang.String[] into java.lang.Integer[]
//   t07d [null, null]
//   t08 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of java.lang.Number[] to the type of the destination array, java.lang.Integer
//   t08d [1, null]
//   t09 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of [Ljava.lang.CharSequence;[] to the type of the destination array, [Ljava.lang.String;
//   t10 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, java.lang.String
//   t11 java.lang.ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of [Ljava.lang.Object;[] to the type of the destination array, [Ljava.lang.String;

import java.util.Arrays;

public class L4W29ArraycopyArrayElements {
    static void row(String id, Object[] src, Object[] dst) {
        try {
            System.arraycopy(src, 0, dst, 0, src.length);
            Object got = dst[0];
            System.out.println(id + " copied; dst[0] is " + (got == null ? "null" : got.getClass().getName()));
        } catch (Throwable t) {
            System.out.println(id + " " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    static void msg(String id, Runnable r) {
        try {
            r.run();
            System.out.println(id + " ok");
        } catch (Throwable t) {
            System.out.println(id + " " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) {
        // An ARRAY element copied into a reference array whose component is
        // that element array's own component (or a supertype of it).
        row("h01", new Object[] {new String[] {"x"}}, new String[1]);
        row("h02", new Object[] {new Integer[] {1}}, new Number[1]);
        row("h03", new Object[] {new String[] {"x"}}, new Comparable[1]);
        row("h04", new Object[] {new String[][] {{"x"}}}, new String[1][]);
        String[] typed = new String[1];
        try {
            System.arraycopy(new Object[] {new String[] {"x"}}, 0, typed, 0, 1);
            String s = typed[0];
            System.out.println("h05 read " + s.length());
        } catch (Throwable t) {
            System.out.println("h05 " + t.getClass().getName());
        }
        // Array elements that ARE assignable.
        row("h06", new Object[] {new String[] {"x"}}, new Object[1][]);
        row("h07", new Object[] {new String[] {"x"}}, new Comparable[1][]);
        row("h08", new Object[] {new int[] {1}}, new Cloneable[1]);
        row("h09", new Object[] {new String[][] {{"x"}}}, new Object[1][]);
        // The two sentences: "type mismatch" unless the destination component
        // is a subtype of the source's.
        msg("t01", () -> System.arraycopy(new String[] {"a"}, 0, new Integer[1], 0, 1));
        Integer[] d1 = new Integer[3];
        msg("t02", () -> System.arraycopy(new Object[] {1, "b"}, 0, d1, 0, 2));
        System.out.println("t02d " + Arrays.toString(d1));
        msg("t03", () -> System.arraycopy(new String[][] {{"a"}}, 0, new Integer[1][], 0, 1));
        msg("t04", () -> System.arraycopy(new Object[][] {new Object[0]}, 0, new String[1][], 0, 1));
        msg("t05", () -> System.arraycopy(new int[][] {new int[0]}, 0, new long[1][], 0, 1));
        msg("t06", () -> System.arraycopy(new Number[] {1}, 0, new String[1], 0, 1));
        Integer[] d2 = new Integer[2];
        msg("t07", () -> System.arraycopy(new String[] {null, "a"}, 0, d2, 0, 2));
        System.out.println("t07d " + Arrays.toString(d2));
        Integer[] d3 = new Integer[2];
        msg("t08", () -> System.arraycopy(new Number[] {1, 2.0}, 0, d3, 0, 2));
        System.out.println("t08d " + Arrays.toString(d3));
        msg("t09", () -> System.arraycopy(new CharSequence[][] {{"a"}, {new StringBuilder("b")}}, 0, new String[2][], 0, 2));
        msg("t10", () -> Arrays.copyOf(new Object[] {"s", 1}, 2, String[].class));
        msg("t11", () -> Arrays.copyOf(new Object[][] {new Object[0]}, 1, String[][].class));
    }
}
