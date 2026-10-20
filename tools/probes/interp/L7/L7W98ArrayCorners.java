// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L7: array semantics (javac output only).
//   ma*   multianewarray: which negative dimension is reported, zero
//         dimensions, partially specified dimensions and the classes of the
//         allocated levels
//   io    instanceof matrix: primitive arrays, arrays of arrays, arrays of
//         interfaces against Object / Cloneable / Serializable / Object[] /
//         Comparable[] / Cloneable[] / Serializable[] / Object[][] / int[] ...
//   cc    checkcast of the same matrix (must agree with instanceof)
//   as*   aastore: the store-check conditions
//   ac*   System.arraycopy: overlap, the partial copy before an
//         ArrayStoreException, the check order of the exceptions
// Every row runs in a 200-round loop as well and must agree with its first
// answer ("rounds agree: true"), so the JIT arm sees warm code too.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L7W98ArrayCorners
// The HotSpot 25 run is the reference (deterministic, no timing).

import java.io.Serializable;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public class L7W98ArrayCorners {
    static int N1 = -1, N2 = -2, Z = 0, TWO = 2, THREE = 3;

    static String t(Throwable e) {
        return e.getClass().getSimpleName() + "(" + e.getMessage() + ")";
    }

    static String multi() {
        StringBuilder sb = new StringBuilder();
        try { int[][][] a = new int[TWO][Z][N1]; sb.append("ma1 ok"); } catch (Throwable e) { sb.append("ma1 ").append(t(e)); }
        try { int[][] a = new int[N1][N2]; sb.append(" ma2 ok"); } catch (Throwable e) { sb.append(" ma2 ").append(t(e)); }
        try { int[][] a = new int[Z][N2]; sb.append(" ma3 ok"); } catch (Throwable e) { sb.append(" ma3 ").append(t(e)); }
        try { String[][][] a = new String[TWO][N1][]; sb.append(" ma4 ok"); } catch (Throwable e) { sb.append(" ma4 ").append(t(e)); }
        int[][][] p = new int[TWO][THREE][];
        sb.append(" ma5 ").append(p.getClass().getName()).append(' ').append(p[1].getClass().getName())
            .append(' ').append(p[1].length).append(' ').append(p[1][2]);
        Object[][] q = new Object[Z][THREE];
        sb.append(" ma6 ").append(q.length).append(' ').append(q.getClass().getName());
        Runnable[][] r = new Runnable[TWO][TWO];
        sb.append(" ma7 ").append(r[0].getClass().getName()).append(' ').append(r[0].getClass().getComponentType().getName());
        long[][] l = new long[TWO][TWO];
        sb.append(" ma8 ").append(l[1].getClass().getName()).append(' ').append(l[1][1]);
        try { Object o = new Object[N1]; sb.append(" ma9 ok"); } catch (Throwable e) { sb.append(" ma9 ").append(t(e)); }
        try { Object o = new int[N2]; sb.append(" ma10 ok"); } catch (Throwable e) { sb.append(" ma10 ").append(t(e)); }
        return sb.toString();
    }

    static final Object[] SUBJECTS = {
        new int[0], new int[0][0], new String[0], new String[0][0], new Object[0], new Integer[0],
        new Runnable[0], new Object[0][0], new Cloneable[0], new long[0][], new Serializable[0][0],
    };

    static String io(Object o) {
        return ""
            + (o instanceof Object ? 1 : 0)
            + (o instanceof Cloneable ? 1 : 0)
            + (o instanceof Serializable ? 1 : 0)
            + (o instanceof Object[] ? 1 : 0)
            + (o instanceof Comparable[] ? 1 : 0)
            + (o instanceof CharSequence[] ? 1 : 0)
            + (o instanceof Object[][] ? 1 : 0)
            + (o instanceof Cloneable[] ? 1 : 0)
            + (o instanceof Serializable[] ? 1 : 0)
            + (o instanceof int[] ? 1 : 0)
            + (o instanceof long[] ? 1 : 0)
            + (o instanceof Number[] ? 1 : 0)
            + (o instanceof Runnable[] ? 1 : 0)
            + (o instanceof long[][] ? 1 : 0)
            + (o instanceof Cloneable[][] ? 1 : 0);
    }

    static int cc(Object o, int which) {
        try {
            switch (which) {
                case 0: { Object x = (Object) o; break; }
                case 1: { Cloneable x = (Cloneable) o; break; }
                case 2: { Serializable x = (Serializable) o; break; }
                case 3: { Object[] x = (Object[]) o; break; }
                case 4: { Comparable[] x = (Comparable[]) o; break; }
                case 5: { CharSequence[] x = (CharSequence[]) o; break; }
                case 6: { Object[][] x = (Object[][]) o; break; }
                case 7: { Cloneable[] x = (Cloneable[]) o; break; }
                case 8: { Serializable[] x = (Serializable[]) o; break; }
                case 9: { int[] x = (int[]) o; break; }
                case 10: { long[] x = (long[]) o; break; }
                case 11: { Number[] x = (Number[]) o; break; }
                case 12: { Runnable[] x = (Runnable[]) o; break; }
                case 13: { long[][] x = (long[][]) o; break; }
                default: { Cloneable[][] x = (Cloneable[][]) o; break; }
            }
            return 1;
        } catch (ClassCastException e) {
            return 0;
        }
    }

    static String matrix() {
        StringBuilder sb = new StringBuilder("io");
        for (Object o : SUBJECTS) sb.append(' ').append(io(o));
        sb.append(" | cc");
        for (Object o : SUBJECTS) {
            sb.append(' ');
            for (int w = 0; w < 15; w++) sb.append(cc(o, w));
        }
        return sb.toString();
    }

    static String store(Object[] a, Object v) {
        try { a[0] = v; return "ok"; } catch (Throwable e) { return t(e); }
    }

    static String aastore() {
        return "as1 " + store(new String[1], Integer.valueOf(1))
            + " as2 " + store(new String[1], null)
            + " as3 " + store(new Number[1], Integer.valueOf(1))
            + " as4 " + store(new Integer[1][], new String[0])
            + " as5 " + store(new Object[1][], new int[0])
            + " as6 " + store(new Object[1][], new String[0])
            + " as7 " + store(new Cloneable[1], new int[0])
            + " as8 " + store(new Serializable[1], new int[0][0])
            + " as9 " + store(new Comparable[1], new Object())
            + " as10 " + store(new Object[1][][], new Object[0][0])
            + " as11 " + store(new Object[1][][], new int[0][0])
            + " as12 " + store(new Object[1][][], new Object[0])
            + " as13 " + store(new Runnable[1], (Runnable) () -> { })
            + " as14 " + store(new Cloneable[1][], new int[0][0]);
    }

    static String copy(Object src, int sp, Object dst, int dp, int len) {
        try { System.arraycopy(src, sp, dst, dp, len); return "ok"; } catch (Throwable e) { return t(e); }
    }

    static String arraycopy() {
        StringBuilder sb = new StringBuilder();
        int[] a = {0, 1, 2, 3, 4, 5, 6, 7};
        System.arraycopy(a, 0, a, 2, 5);
        sb.append("ac1 ").append(Arrays.toString(a));
        int[] b = {0, 1, 2, 3, 4, 5, 6, 7};
        System.arraycopy(b, 2, b, 0, 5);
        sb.append(" ac2 ").append(Arrays.toString(b));
        String[] c = {"a", "b", "c", "d", "e"};
        System.arraycopy(c, 0, c, 1, 4);
        sb.append(" ac3 ").append(Arrays.toString(c));
        Object[] src = {"a", "b", Integer.valueOf(1), "c"};
        String[] dst = new String[4];
        sb.append(" ac4 ").append(copy(src, 0, dst, 0, 4)).append(' ').append(Arrays.toString(dst));
        Number[] ns = {Integer.valueOf(1), Double.valueOf(2), Integer.valueOf(3)};
        Integer[] is = new Integer[3];
        sb.append(" ac5 ").append(copy(ns, 0, is, 0, 3)).append(' ').append(Arrays.toString(is));
        sb.append(" ac6 ").append(copy(new int[4], 0, new long[4], 0, 9));
        sb.append(" ac7 ").append(copy(new int[4], 0, new Object[4], 0, 1));
        sb.append(" ac8 ").append(copy(new int[4], 0, new int[4], 0, -1));
        sb.append(" ac9 ").append(copy(new int[4], 4, new int[4], 0, 0));
        sb.append(" ac10 ").append(copy(new int[4], 5, new int[4], 0, 0));
        sb.append(" ac11 ").append(copy(null, 0, new int[4], 0, 0));
        sb.append(" ac12 ").append(copy(new int[4], 0, null, 0, 0));
        sb.append(" ac13 ").append(copy("str", 0, new int[4], 0, 0));
        sb.append(" ac14 ").append(copy(new int[4], 0, "str", 0, 0));
        sb.append(" ac15 ").append(copy(new String[2], 0, new Integer[2], 0, 2));
        sb.append(" ac16 ").append(copy(new String[] {"x"}, 0, new Integer[2], 0, 1));
        sb.append(" ac17 ").append(copy(new String[2], 0, new Integer[2], 0, 3));
        sb.append(" ac18 ").append(copy(new int[2][], 0, new long[2][], 0, 2));
        sb.append(" ac19 ").append(copy(new int[][] {new int[1]}, 0, new long[2][], 0, 1));
        sb.append(" ac20 ").append(copy(new Object[] {new int[1]}, 0, new Object[1][], 0, 1));
        Object[] same = {"a", "b", "c"};
        sb.append(" ac21 ").append(copy(same, 1, same, 0, 2)).append(' ').append(Arrays.toString(same));
        sb.append(" ac22 ").append(copy(new int[4], 0, new int[4], Integer.MAX_VALUE, 1));
        return sb.toString();
    }

    static String misc() {
        StringBuilder sb = new StringBuilder();
        int[] n = null;
        try { sb.append("len ").append(n.length); } catch (Throwable e) { sb.append("len ").append(e.getClass().getSimpleName()); }
        int[] a = new int[TWO];
        try { a[N1] = 1; sb.append(" idx ok"); } catch (Throwable e) { sb.append(" idx ").append(t(e)); }
        try { sb.append(" idx2 ").append(a[TWO]); } catch (Throwable e) { sb.append(" idx2 ").append(t(e)); }
        Object[] o = new String[TWO];
        try { o[TWO] = Integer.valueOf(1); sb.append(" idx3 ok"); } catch (Throwable e) { sb.append(" idx3 ").append(t(e)); }
        Object[] on = null;
        try { on[0] = Integer.valueOf(1); sb.append(" npe ok"); } catch (Throwable e) { sb.append(" npe ").append(e.getClass().getSimpleName()); }
        boolean[] z = new boolean[1];
        byte[] by = new byte[1];
        char[] ch = new char[1];
        short[] sh = new short[1];
        z[0] = true; by[0] = (byte) 200; ch[0] = (char) -1; sh[0] = (short) 70000;
        sb.append(" prim ").append(z[0]).append(' ').append(by[0]).append(' ').append((int) ch[0]).append(' ').append(sh[0]);
        int[] cl = a.clone();
        sb.append(" clone ").append(cl != a).append(' ').append(cl.getClass() == a.getClass());
        Object[][] deep = new String[1][1];
        Object[][] dc = deep.clone();
        sb.append(" clone2 ").append(dc.getClass().getName()).append(' ').append(dc[0] == deep[0]);
        return sb.toString();
    }

    static List<String> all() {
        List<String> l = new ArrayList<>();
        l.add(multi());
        l.add(matrix());
        l.add(aastore());
        l.add(arraycopy());
        l.add(misc());
        return l;
    }

    public static void main(String[] args) {
        List<String> first = all();
        for (String row : first) System.out.println(row);
        boolean agree = true;
        for (int round = 0; round < 200; round++) {
            List<String> again = all();
            for (int i = 0; i < first.size(); i++) {
                if (!first.get(i).equals(again.get(i))) {
                    if (agree) System.out.println("round " + round + " differs: " + again.get(i));
                    agree = false;
                }
            }
        }
        System.out.println("rounds agree: " + agree);
    }
}
