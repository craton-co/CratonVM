// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.WrongMethodTypeException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * SBR wave 6, lane {@code dispatch}: signature-polymorphic {@code MethodHandle.invokeExact}/
 * {@code invoke} answers, exercised so that the dispatch's per-(receiver class, call-site
 * descriptor) memo is warm when the answer matters. Each row prints its first-run value and is
 * re-run in a hot loop, so cold == warm is asserted ({@code drift}). Rows cover: one call site
 * over several handle species (direct, bound, adapter, collector), the same handle at call sites
 * of different static types (exact refusal after the memo is warm, {@code invoke}'s widening and
 * boxing), varargs null semantics, exceptions from the target, void and reference returns.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW6Poly [hotIters]
 * </pre>
 */
public class SbrW6Poly {
    static int inc(int a) { return a + 1; }
    static long incL(long a) { return a + 1; }
    static String cat(String a, String b) { return a + b; }
    static String join(String... xs) { return String.valueOf(Arrays.toString(xs)); }
    static void sink(int a) { last = a; }
    static int last;
    static int boom(int a) { throw new IllegalStateException("boom" + a); }
    int f = 5;
    int addF(int a) { return a + f; }

    interface Row { String run() throws Throwable; }
    static final List<String> names = new ArrayList<>();
    static final List<Row> rows = new ArrayList<>();
    static void add(String n, Row r) { names.add(n); rows.add(r); }

    static String msg(Throwable t) {
        return t.getClass().getSimpleName() + ": " + t.getMessage();
    }

    public static void main(String[] args) throws Throwable {
        int hot = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle mInc = lk.findStatic(SbrW6Poly.class, "inc", MethodType.methodType(int.class, int.class));
        MethodHandle mIncL = lk.findStatic(SbrW6Poly.class, "incL", MethodType.methodType(long.class, long.class));
        MethodHandle mCat = lk.findStatic(SbrW6Poly.class, "cat", MethodType.methodType(String.class, String.class, String.class));
        MethodHandle mJoin = lk.findStatic(SbrW6Poly.class, "join", MethodType.methodType(String.class, String[].class));
        MethodHandle mSink = lk.findStatic(SbrW6Poly.class, "sink", MethodType.methodType(void.class, int.class));
        MethodHandle mBoom = lk.findStatic(SbrW6Poly.class, "boom", MethodType.methodType(int.class, int.class));
        MethodHandle mAddF = lk.findVirtual(SbrW6Poly.class, "addF", MethodType.methodType(int.class, int.class));
        MethodHandle mGet = lk.findGetter(SbrW6Poly.class, "f", int.class);
        MethodHandle bound = mAddF.bindTo(new SbrW6Poly());
        MethodHandle ins = MethodHandles.insertArguments(mCat, 0, "pre-");
        MethodHandle dropped = MethodHandles.dropArguments(mInc, 1, String.class);
        MethodHandle filtered = MethodHandles.filterReturnValue(mInc, mInc);
        MethodHandle[] species = {mInc, bound, filtered, MethodHandles.identity(int.class)};

        add("static exact", () -> "" + (int) mInc.invokeExact(41));
        add("virtual exact", () -> "" + (int) mAddF.invokeExact(new SbrW6Poly(), 1));
        add("getter exact", () -> "" + (int) mGet.invokeExact(new SbrW6Poly()));
        add("bound exact", () -> "" + (int) bound.invokeExact(10));
        add("insertArguments exact", () -> (String) ins.invokeExact("x"));
        add("dropArguments exact", () -> "" + (int) dropped.invokeExact(3, "ignored"));
        add("filterReturnValue exact", () -> "" + (int) filtered.invokeExact(3));
        add("void exact", () -> { mSink.invokeExact(77); return "" + last; });
        add("ref exact", () -> (String) mCat.invokeExact("a", "b"));
        add("one site, four species", () -> {
            StringBuilder sb = new StringBuilder();
            for (MethodHandle h : species) sb.append((int) h.invokeExact(5)).append(',');
            return sb.toString();
        });
        add("exact refusal (long site over int handle)", () -> {
            try {
                return "" + (long) mInc.invokeExact(1L);
            } catch (Throwable t) {
                return msg(t);
            }
        });
        add("exact refusal (Object return)", () -> {
            try {
                return "" + (Object) mInc.invokeExact(1);
            } catch (Throwable t) {
                return msg(t);
            }
        });
        add("exact then invoke widening", () -> {
            long w = (long) mInc.invoke(1);
            Object o = (Object) mInc.invoke(2);
            return w + "/" + o;
        });
        add("invoke boxing to Integer", () -> "" + (Integer) mInc.invoke(Integer.valueOf(9)));
        add("invoke long handle with int arg", () -> "" + (long) mIncL.invoke(4));
        add("invoke varargs spread", () -> (String) mJoin.invoke("a", "b", "c"));
        add("invoke varargs null String[]", () -> (String) mJoin.invoke((String[]) null));
        add("invoke varargs null Object", () -> (String) mJoin.invoke((Object) null));
        add("invokeExact varargs array", () -> (String) mJoin.invokeExact(new String[] {"x", "y"}));
        add("target throws", () -> {
            try {
                return "" + (int) mBoom.invokeExact(3);
            } catch (Throwable t) {
                return msg(t);
            }
        });
        add("null receiver", () -> {
            try {
                MethodHandle n = null;
                return "" + (int) n.invokeExact(3);
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        });
        add("invoke arity refusal", () -> {
            try {
                return "" + (int) mInc.invoke(1, 2);
            } catch (WrongMethodTypeException t) {
                return msg(t);
            }
        });
        add("lambda call site", () -> ((java.util.function.IntSupplier) () -> {
            try {
                return (int) mInc.invokeExact(6);
            } catch (Throwable t) {
                throw new RuntimeException(t);
            }
        }).getAsInt() + "");

        if (args.length > 1) {
            for (int i = rows.size() - 1; i >= 0; i--) {
                if (!names.get(i).contains(args[1])) { names.remove(i); rows.remove(i); }
            }
        }
        String[] first = new String[rows.size()];
        for (int i = 0; i < rows.size(); i++) {
            try {
                first[i] = rows.get(i).run();
            } catch (Throwable t) {
                first[i] = "THROW " + msg(t);
            }
            System.out.println("row " + names.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int[] d = new int[rows.size()];
        for (int it = 0; it < hot; it++) {
            for (int i = 0; i < rows.size(); i++) {
                String v;
                try {
                    v = rows.get(i).run();
                } catch (Throwable t) {
                    v = "THROW " + msg(t);
                }
                if (!v.equals(first[i])) {
                    d[i]++;
                    drift++;
                }
            }
        }
        for (int i = 0; i < rows.size(); i++) if (d[i] != 0) System.out.println("drift " + names.get(i) + " x" + d[i]);
        System.out.println("drift " + drift);
    }
}
