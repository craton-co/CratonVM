// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L4: `VarHandle.withInvokeExactBehavior`
// (docs/internal/fixed-bugs/interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010.md,
// item 3). An exact handle is a NEW handle whose `hasInvokeExactBehavior()`
// is true; an access whose call-site type is not the access mode type throws
// `WrongMethodTypeException("handle's method type T but found S")`, where a
// plain handle converts.
//
//   flags-*    the builder pair and the flag
//   *-ok       an exactly typed access on the exact handle answers
//   *-wmte     a mistyped access on the exact handle throws
//   plain-*    the same mistyped access on the plain handle converts
//   hot-*      the mistyped and the exact access 20 000 times from one
//              method, so a compiled caller is measured too
//
// Before wave 37 `withInvokeExactBehavior()` returned `this`: every `flags-*`
// row but the `plain` ones and every `*-wmte` row differed. `--compatible`
// keeps that (by design: the exact copy is minted under `--jdk-only` only).
//
// Known open half (the page's Progress (wave 37)): a mistyped access from a
// COMPILED site the JIT bound to a thin VarHandle helper is not judged (that
// helper keeps no call-site descriptor), so `hot-wrong` may print less than
// 20000 in the default (JIT) mode; `--nojit` matches every row.
//
// Positive control: CRATONVM_DBG_MH_STACK=1 prints
// `[MH_STACK] exact VarHandle get(LL4W37VarHandleExact$H;)J: refused` for the
// `get-wmte` row (and `admitted` for every judged access that passes).
//
// Run: javac -d out L4W37VarHandleExact.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W37VarHandleExact
//
// Expected HotSpot 25 output (default and -Xint):
//   flags-plain: false
//   flags-exact: true
//   flags-distinct: true
//   flags-exact-again-same: true
//   flags-plain-again-same: true
//   flags-back-to-plain: false
//   flags-var-type: int [class L4W37VarHandleExact$H]
//   get-ok: 3
//   get-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H)int but found (H)long
//   get-object-coordinate-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H)int but found (Object)int
//   get-boxed-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H)int but found (H)Integer
//   plain-get-long: 3
//   getvolatile-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H)int but found (H)long
//   set-ok: 5
//   set-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H,int)void but found (H,Integer)void
//   set-short-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H,int)void but found (H,short)void
//   plain-set-short: 6
//   cas-ok: true
//   cas-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H,int,int)boolean but found (H,long,int)boolean
//   getandadd-ok: 3
//   getandadd-void-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H,int)int but found (H,int)void
//   static-ok: 7
//   static-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type ()int but found ()Object
//   array-ok: 2
//   array-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (int[],int)int but found (int[],long)int
//   ref-ok: o
//   ref-string-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H)Object but found (H)String
//   ref-set-string-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (H,Object)void but found (H,String)void
//   hot-wrong: 20000
//   hot-right: 60000
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.invoke.WrongMethodTypeException;

public class L4W37VarHandleExact {
    static class H {
        int x = 3;
        long l = 4L;
        Object o = "o";
        static int s = 7;
    }

    static final VarHandle X;
    static final VarHandle X_EXACT;
    static final VarHandle S_EXACT;
    static final VarHandle A_EXACT;
    static final VarHandle O_EXACT;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            X = l.findVarHandle(H.class, "x", int.class);
            X_EXACT = X.withInvokeExactBehavior();
            S_EXACT = l.findStaticVarHandle(H.class, "s", int.class).withInvokeExactBehavior();
            A_EXACT = MethodHandles.arrayElementVarHandle(int[].class).withInvokeExactBehavior();
            O_EXACT = l.findVarHandle(H.class, "o", Object.class).withInvokeExactBehavior();
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static int hotWrong(H h, int n) {
        int thrown = 0;
        for (int i = 0; i < n; i++) {
            try {
                long v = (long) X_EXACT.get(h);
                if (v != 3) {
                    return -1;
                }
            } catch (WrongMethodTypeException e) {
                thrown++;
            }
        }
        return thrown;
    }

    static long hotRight(H h, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (int) X_EXACT.get(h);
            X_EXACT.set(h, 3);
        }
        return sum;
    }

    public static void main(String[] args) {
        H h = new H();
        int[] arr = {1, 2, 3};
        row("flags-plain", () -> X.hasInvokeExactBehavior());
        row("flags-exact", () -> X_EXACT.hasInvokeExactBehavior());
        row("flags-distinct", () -> X_EXACT != X);
        row("flags-exact-again-same", () -> X_EXACT.withInvokeExactBehavior() == X_EXACT);
        row("flags-plain-again-same", () -> X.withInvokeBehavior() == X);
        row("flags-back-to-plain", () -> X_EXACT.withInvokeBehavior().hasInvokeExactBehavior());
        row("flags-var-type", () -> X_EXACT.varType() + " " + X_EXACT.coordinateTypes());
        row("get-ok", () -> (int) X_EXACT.get(h));
        row("get-wmte", () -> (long) X_EXACT.get(h));
        row("get-object-coordinate-wmte", () -> (int) X_EXACT.get((Object) h));
        row("get-boxed-wmte", () -> (Integer) X_EXACT.get(h));
        row("plain-get-long", () -> (long) X.get(h));
        row("getvolatile-wmte", () -> (long) X_EXACT.getVolatile(h));
        row("set-ok", () -> {
            X_EXACT.set(h, 5);
            return h.x;
        });
        row("set-wmte", () -> {
            X_EXACT.set(h, Integer.valueOf(6));
            return h.x;
        });
        row("set-short-wmte", () -> {
            X_EXACT.set(h, (short) 6);
            return h.x;
        });
        row("plain-set-short", () -> {
            X.set(h, (short) 6);
            return h.x;
        });
        row("cas-ok", () -> (boolean) X_EXACT.compareAndSet(h, 6, 3));
        row("cas-wmte", () -> (boolean) X_EXACT.compareAndSet(h, 3L, 3));
        row("getandadd-ok", () -> (int) X_EXACT.getAndAdd(h, 0));
        row("getandadd-void-wmte", () -> {
            X_EXACT.getAndAdd(h, 0);
            return "void";
        });
        row("static-ok", () -> (int) S_EXACT.get());
        row("static-wmte", () -> (Object) S_EXACT.get());
        row("array-ok", () -> (int) A_EXACT.get(arr, 1));
        row("array-wmte", () -> (int) A_EXACT.get(arr, 1L));
        row("ref-ok", () -> (Object) O_EXACT.get(h));
        row("ref-string-wmte", () -> (String) O_EXACT.get(h));
        row("ref-set-string-wmte", () -> {
            O_EXACT.set(h, "p");
            return h.o;
        });
        row("hot-wrong", () -> hotWrong(h, 20_000));
        row("hot-right", () -> hotRight(h, 20_000));
    }
}
