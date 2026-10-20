// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: TYPECHECK21-3 for `checkcast`
// (`CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS`: `Op::CheckCast` joins its hit; the
// helper keeps every miss and the ClassCastException).
//
// Risk targeted: a false HIT of the array arm on checkcast is a silent
// heap-pollution bug (a `String[][]` accepted as a `String[]`, an `int[]` as
// an `Object[]`): the cast succeeds and a later element read returns the
// wrong type. A false MISS is only slow. Each row casts in a hot method (one
// checkcast site per target), then USES the result (reads element 0's class,
// stores an element back) so a wrong hit surfaces as a later CCE / ASE or a
// wrong class name. The CCE message (HotSpot's "class X cannot be cast to
// class Y (...)" text) is printed for every miss. Cold, hot, after a deopt
// (a late component class).
//
// Run:
//   javac -d out R21FzTcArray2.java
//   java -Xmx256m -cp out R21FzTcArray2                                   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzTcArray2
//   CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS=1 cratonvm -Xmx256m -cp out R21FzTcArray2
//   CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS=1 CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY=1 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzTcArray2 {
    static final int WARM = 50_000;

    interface Tag {
    }

    static class Base implements Tag {
    }

    static class Sub extends Base {
    }

    static class Late extends Sub {
    }

    static String c0(Object o) {
        Object[] a = (Object[]) o;
        return a.length == 0 || a[0] == null ? "len" + a.length : a[0].getClass().getName();
    }

    static String c1(Object o) {
        Base[] a = (Base[]) o;
        return a.length == 0 || a[0] == null ? "len" + a.length : a[0].getClass().getName();
    }

    static String c2(Object o) {
        Tag[] a = (Tag[]) o;
        return a.length == 0 || a[0] == null ? "len" + a.length : a[0].getClass().getName();
    }

    static String c3(Object o) {
        String[] a = (String[]) o;
        return a.length == 0 || a[0] == null ? "len" + a.length : a[0];
    }

    static String c4(Object o) {
        Object[][] a = (Object[][]) o;
        return a.length == 0 || a[0] == null ? "len" + a.length : "inner" + a[0].length;
    }

    static String c5(Object o) {
        Sub[] a = (Sub[]) o;
        if (a.length > 0) {
            a[0] = new Sub();
        }
        return "stored" + a.length;
    }

    static String c6(Object o) {
        Comparable<?>[] a = (Comparable<?>[]) o;
        return a.length == 0 || a[0] == null ? "len" + a.length : a[0].getClass().getName();
    }

    interface Cast {
        String apply(Object o);
    }

    static final Cast[] CASTS = {R21FzTcArray2::c0, R21FzTcArray2::c1, R21FzTcArray2::c2, R21FzTcArray2::c3,
        R21FzTcArray2::c4, R21FzTcArray2::c5, R21FzTcArray2::c6};

    static String one(int k, Object o) {
        try {
            return CASTS[k].apply(o);
        } catch (ClassCastException e) {
            return "CCE(" + e.getMessage() + ")";
        } catch (ArrayStoreException e) {
            return "ASE(" + e.getMessage() + ")";
        } catch (NullPointerException e) {
            return "NPE";
        }
    }

    static Object[] values() {
        return new Object[] {
            null, new Object[] {"x"}, new Base[] {new Sub()}, new Sub[] {new Sub()}, new Tag[] {new Base()},
            new String[] {"str"}, new String[][] {{"a"}}, new int[] {1}, new int[][] {{1}}, new Object[][] {{}},
            new Base[][] {{new Base()}}, new Integer[] {7}, new Sub[0], new Object(), "plain",
        };
    }

    static void rows(String tag, Object[] vs) {
        for (int i = 0; i < vs.length; i++) {
            StringBuilder sb = new StringBuilder("row " + tag + " v" + i);
            for (int k = 0; k < CASTS.length; k++) {
                sb.append(" | ").append(one(k, vs[i]));
            }
            System.out.println(sb);
        }
    }

    public static void main(String[] args) {
        Object[] vs = values();
        rows("cold", vs);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            Object v = vs[r % vs.length];
            for (int k = 0; k < CASTS.length; k++) {
                h = h * 31 + one(k, v).length();
            }
        }
        System.out.println("warm hash " + h);
        rows("hot", vs);
        Object[] late = {new Late[] {new Late()}, new Late[][] {{new Late()}}};
        rows("late", late);
        rows("after", vs);
    }
}
