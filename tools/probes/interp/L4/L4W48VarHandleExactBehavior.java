// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L4 (bug hunt; page
// `docs/internal/fixed-bugs/interpreter-L4-varhandle-invoke-exact-behavior-is-not-enforced-FIXED-20261013.md`):
// a VarHandle with invoke-exact behaviour (`withInvokeExactBehavior()`)
// refuses an access whose symbolic type is not the access mode's exact type
// (`WrongMethodTypeException`). CratonVM reported the behaviour
// (`hasInvokeExactBehavior()` is `true`) but converted as for a plain handle.
// Fixed in wave 49 (lane L4); every mode matches.
//
// Run: javac -d out L4W48VarHandleExactBehavior.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W48VarHandleExactBehavior
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   warm-cas: true 9
//   warm-flag: false true
//   warm-get-object: java.lang.invoke.WrongMethodTypeException: handle's method type (int[],int)int but found (int[],int)Object
//   flag: false true false
//   get-object: java.lang.invoke.WrongMethodTypeException: handle's method type (int[],int)int but found (int[],int)Object
//   get-exact: 7
//   set-long: java.lang.invoke.WrongMethodTypeException: handle's method type (int[],int,int)void but found (int[],int,long)void
//   field-cas-boxed: java.lang.invoke.WrongMethodTypeException: handle's method type (L4W48VarHandleExactBehavior,int,int)boolean but found (L4W48VarHandleExactBehavior,Integer,int)boolean
//   plain-get-object: 7
//   fresh-get-object: java.lang.invoke.WrongMethodTypeException: handle's method type (int[],int)int but found (int[],int)Object
//   fresh-long-get-object: java.lang.invoke.WrongMethodTypeException: handle's method type (long[],int)long but found (long[],int)Object
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W48VarHandleExactBehavior {
    int x = 7;

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        // The wave-48 hunt's order: an `int[]` element handle used plainly
        // (a CAS) before any exact one.
        row("warm-cas", () -> {
            VarHandle vh = MethodHandles.arrayElementVarHandle(int[].class);
            int[] w = {1, 2};
            return vh.compareAndSet(w, 1, 2, 9) + " " + w[1];
        });
        row("warm-flag", () -> {
            VarHandle vh = MethodHandles.arrayElementVarHandle(int[].class);
            return vh.hasInvokeExactBehavior() + " " + vh.withInvokeExactBehavior().hasInvokeExactBehavior();
        });
        row("warm-get-object", () -> {
            VarHandle vh = MethodHandles.arrayElementVarHandle(int[].class).withInvokeExactBehavior();
            int[] w = {1};
            Object o = vh.get(w, 0);
            return o;
        });
        VarHandle plain = MethodHandles.arrayElementVarHandle(int[].class);
        VarHandle exact = plain.withInvokeExactBehavior();
        VarHandle field = MethodHandles.lookup()
            .findVarHandle(L4W48VarHandleExactBehavior.class, "x", int.class).withInvokeExactBehavior();
        int[] a = {7};
        row("flag", () -> plain.hasInvokeExactBehavior() + " " + exact.hasInvokeExactBehavior()
            + " " + exact.withInvokeBehavior().hasInvokeExactBehavior());
        row("get-object", () -> {
            Object o = exact.get(a, 0);
            return o;
        });
        row("get-exact", () -> (int) exact.get(a, 0));
        row("set-long", () -> {
            exact.set(a, 0, 5L);
            return a[0];
        });
        row("field-cas-boxed", () -> {
            L4W48VarHandleExactBehavior self = new L4W48VarHandleExactBehavior();
            return (boolean) field.compareAndSet(self, Integer.valueOf(7), 8);
        });
        row("plain-get-object", () -> {
            Object o = plain.get(a, 0);
            return o;
        });
        row("fresh-get-object", () -> {
            VarHandle vh = MethodHandles.arrayElementVarHandle(int[].class).withInvokeExactBehavior();
            int[] b = {1};
            Object o = vh.get(b, 0);
            return o;
        });
        row("fresh-long-get-object", () -> {
            VarHandle vh = MethodHandles.arrayElementVarHandle(long[].class).withInvokeExactBehavior();
            long[] b = {1};
            Object o = vh.get(b, 0);
            return o;
        });
    }
}
