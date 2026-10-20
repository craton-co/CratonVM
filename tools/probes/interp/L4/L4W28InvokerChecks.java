// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 28, lane L4: an invoker handle
// (`MethodHandles.exactInvoker` / `invoker` / `spreadInvoker`) checks an
// `invokeExact` call site against its OWN type, an exact invoker checks that
// its target's type is exactly `t` (`Invokers.checkExactType`), a plain or
// spread invoker converts its target to `t` (`checkGenericType`, an
// `asType(t)`) or refuses it, and a null target is HotSpot's helpful NPE.
// Rows 17-19: an `explicitCastArguments` stamp passes a reference to an
// INTERFACE parameter uncast through every door (round 12's `invokeExact`
// inner cast and the `invoke` door's leaf pass refused it). Rows 30-31: a
// `findSpecial` handle beneath an adapter casts its receiver to the
// `specialCaller` round 12 records. Rows 32-36: a reference return is cast
// to an interface by `asType` (only an explicit cast skips it). Page (all):
// docs/internal/fixed-bugs/interpreter-L4-an-exact-invoker-checks-neither-its-call-site-nor-its-targets-type-FIXED-20260929.md
//
// Each row runs WARM times in `drive` (the call site is in the row's lambda,
// so the JIT compiles it); a row prints its first outcome and, if any later
// iteration answered differently, how many did. Rows 16-16s and 39 run once
// (`once`).
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W28InvokerChecks
// (all four combinations must print exactly HotSpot's lines)
//
// HotSpot 25 (25.0.3, default and -Xint) prints, verbatim:
//   1 exact site one short: java.lang.invoke.WrongMethodTypeException: handle's method type (MethodHandle,Object[])Object but found (MethodHandle,Object)Object
//   2 exact site wrong return: java.lang.invoke.WrongMethodTypeException: handle's method type (MethodHandle,Object[])Object but found (MethodHandle,Object[])String
//   3 exact target of another type: java.lang.invoke.WrongMethodTypeException: handle's method type (Object[])String but found (Object[])Object
//   4 exact target of type t: a String
//   5 plain target converted: a String
//   6 plain site one short: java.lang.invoke.WrongMethodTypeException: handle's method type (MethodHandle,Object[])Object but found (MethodHandle,Object)Object
//   7 invoke on exact, target of another type: java.lang.invoke.WrongMethodTypeException: handle's method type (Object[])String but found (Object[])Object
//   8 invoke on exact, argument cast: a String
//   9 plain boxes an int result: 7 Integer
//   10 exact int target at Object type: java.lang.invoke.WrongMethodTypeException: handle's method type (int,int)int but found (Object,Object)Object
//   11 plain inconvertible target: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String)String to (int)int
//   12 plain widens an int result: 3 Long
//   13 exact int target at long type: java.lang.invoke.WrongMethodTypeException: handle's method type (int,int)int but found (int,int)long
//   14 invoke narrows the invoker: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(MethodHandle,int,int)int to (MethodHandle,long,int)int
//   15 exact null receiver: java.lang.NullPointerException: null
//   16 exact null target: java.lang.NullPointerException: Cannot invoke "java.lang.invoke.MethodHandle.type()" because "mh" is null
//   16p plain null target: java.lang.NullPointerException: Cannot invoke "java.lang.invoke.MethodHandle.asType(java.lang.invoke.MethodType)" because "mh" is null
//   16s spread null target: java.lang.NullPointerException: Cannot invoke "java.lang.invoke.MethodHandle.asType(java.lang.invoke.MethodType)" because "mh" is null
//   17 explicitCast to interface invokeExact: s String
//   18 explicitCast to interface invoke: s String
//   19 explicitCast to interface invokeWithArguments: s String
//   20 asType to interface invokeExact: java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Runnable
//   21 plain invoker over explicitCast: s String
//   22 plain Integer parameter unboxed: 7 Integer
//   23 plain Integer result unboxed: 9 Integer
//   24 plain void result at Object: null
//   25 plain array parameter cast: java.lang.ClassCastException: Cannot cast java.lang.String to [Ljava.lang.Object;
//   26 exact invokeWithArguments: a String
//   27 exact invokeWithArguments target of another type: java.lang.invoke.WrongMethodTypeException: handle's method type (Object[])String but found (Object[])Object
//   28 spread converts its target: a String
//   29 fold on exactInvoker, target of another type: java.lang.invoke.WrongMethodTypeException: handle's method type (Object[])String but found (Object[])Object
//   30 special under adapter, receiver not specialCaller: java.lang.ClassCastException: Cannot cast L4W28InvokerChecks$Mid to L4W28InvokerChecks$Sub
//   31 special under adapter, specialCaller receiver: 2 Integer
//   32 invoke return to interface: java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Runnable
//   33 asType return to interface: java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Runnable
//   34 explicitCast return to interface: s String
//   35 explicitCast return to interface, invoke: s String
//   36 adapter invoke return to interface: java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Runnable
//   37 invoker non-handle target: java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.invoke.MethodHandle
//   38 exactInvoker invokeWithArguments non-handle: java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.invoke.MethodHandle
//   39 insert null target: java.lang.NullPointerException: Cannot invoke "java.lang.invoke.MethodHandle.asType(java.lang.invoke.MethodType)" because "mh" is null
//
// CratonVM before the fix (read from the code, not run; both modes): rows 1,
// 2, 6 answered `a` (an `invokeExact` call site on an invoker was not
// checked); rows 3, 7, 10, 13, 27, 29 ran the target whatever its type (`a`,
// `a`, a raw `7`, `3`); row 9 handed the raw `int` 7 to an `Object` call site
// and row 12 `3` without the widening; row 11 ran `up` with an `int` in its
// `String` parameter; row 14 answered `3`; rows 16-16s answered `null`; rows
// 17-19 threw `ClassCastException: Cannot cast java.lang.String to
// java.lang.Runnable`; row 30 answered `2` (the SPECIAL leaf cast the
// receiver to the method's class, `Mid`, not to `specialCaller`); rows 32,
// 33 and 36 answered `s String` (round 12's return cast skipped every
// interface target); rows 37 and 38 read the `String`'s fields as a
// handle's (an arbitrary answer or a crash), row 39 answered `null`. Rows 4,
// 5, 8, 15, 20-26, 28, 31, 34, 35 are controls. Rows 16-16s and 39 run once.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W28InvokerChecks {
    static final int WARM = 2000;

    interface Body {
        Object run() throws Throwable;
    }

    static Object first(Object[] xs) {
        return xs.length == 0 ? "none" : xs[0];
    }

    static String takesRunnable(Runnable r) {
        return String.valueOf(r);
    }

    static int sum(int a, int b) {
        return a + b;
    }

    static String up(String s) {
        return s.toUpperCase();
    }

    static Integer boxed(int a) {
        return a;
    }

    static void sink(int a) {
    }

    static String outcome(Body b) {
        try {
            Object r = b.run();
            return r == null ? "null" : r + " " + r.getClass().getSimpleName();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void drive(String label, Body b) {
        String first = outcome(b);
        int differing = 0;
        for (int i = 1; i < WARM; i++) {
            if (!first.equals(outcome(b))) {
                differing++;
            }
        }
        System.out.println(label + ": " + first + (differing == 0 ? "" : "  [" + differing + " later iterations differ]"));
    }

    // One run: HotSpot's C2 drops the helpful NPE message of row 16 in
    // compiled code (about half of 2000 warm iterations), so the rows that
    // print one are judged on their first, interpreted, outcome only.
    static void once(String label, Body b) {
        System.out.println(label + ": " + outcome(b));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W28InvokerChecks.class;
        MethodType t = MethodType.methodType(Object.class, Object[].class);
        MethodHandle fst = l.findStatic(me, "first", t);
        MethodHandle fstS = fst.asType(MethodType.methodType(String.class, Object[].class));
        MethodHandle ex = MethodHandles.exactInvoker(t);
        MethodHandle inv = MethodHandles.invoker(t);
        Object[] arr = {"a"};

        // The page's rows: the invoker's own call site, and its target's type.
        drive("1 exact site one short", () -> (Object) ex.invokeExact(fst, (Object) new Object[] {"a"}));
        drive("2 exact site wrong return", () -> (String) ex.invokeExact(fst, new Object[] {"a"}));
        drive("3 exact target of another type", () -> (Object) ex.invokeExact(fstS, arr));
        drive("4 exact target of type t", () -> (Object) ex.invokeExact(fst, arr));
        drive("5 plain target converted", () -> (Object) inv.invokeExact(fstS, arr));
        drive("6 plain site one short", () -> (Object) inv.invokeExact(fst, (Object) arr));
        drive("7 invoke on exact, target of another type", () -> (Object) ex.invoke(fstS, arr));
        drive("8 invoke on exact, argument cast", () -> (Object) ex.invoke(fst, (Object) arr));

        // A plain invoker is `target.asType(t)`; an exact one refuses.
        MethodHandle s = l.findStatic(me, "sum", MethodType.methodType(int.class, int.class, int.class));
        MethodType tObj = MethodType.methodType(Object.class, Object.class, Object.class);
        MethodHandle invObj = MethodHandles.invoker(tObj);
        MethodHandle exObj = MethodHandles.exactInvoker(tObj);
        drive("9 plain boxes an int result", () -> (Object) invObj.invokeExact(s, (Object) 3, (Object) 4));
        drive("10 exact int target at Object type", () -> (Object) exObj.invokeExact(s, (Object) 3, (Object) 4));
        MethodHandle upH = l.findStatic(me, "up", MethodType.methodType(String.class, String.class));
        MethodHandle invInt = MethodHandles.invoker(MethodType.methodType(int.class, int.class));
        drive("11 plain inconvertible target", () -> (int) invInt.invokeExact(upH, 5));
        MethodType tLong = MethodType.methodType(long.class, int.class, int.class);
        MethodHandle invLong = MethodHandles.invoker(tLong);
        MethodHandle exLong = MethodHandles.exactInvoker(tLong);
        drive("12 plain widens an int result", () -> (long) invLong.invokeExact(s, 1, 2));
        drive("13 exact int target at long type", () -> (long) exLong.invokeExact(s, 1, 2));
        MethodHandle invS = MethodHandles.invoker(s.type());
        drive("14 invoke narrows the invoker", () -> (int) invS.invoke(s, 1L, 2));

        // Null receiver, null target.
        MethodHandle len = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
        MethodHandle exLen = MethodHandles.exactInvoker(len.type());
        drive("15 exact null receiver", () -> (int) exLen.invokeExact(len, (String) null));
        once("16 exact null target", () -> (Object) ex.invokeExact((MethodHandle) null, arr));
        once("16p plain null target", () -> (Object) inv.invokeExact((MethodHandle) null, arr));
        MethodHandle sp0 = MethodHandles.spreadInvoker(t, 0);
        once("16s spread null target", () -> (Object) sp0.invokeExact((MethodHandle) null, new Object[] {arr}));

        // `explicitCastArguments` to an interface parameter does not cast.
        MethodHandle tr = l.findStatic(me, "takesRunnable", MethodType.methodType(String.class, Runnable.class));
        MethodHandle trx = MethodHandles.explicitCastArguments(tr, MethodType.methodType(String.class, Object.class));
        drive("17 explicitCast to interface invokeExact", () -> (String) trx.invokeExact((Object) "s"));
        drive("18 explicitCast to interface invoke", () -> (String) trx.invoke((Object) "s"));
        drive("19 explicitCast to interface invokeWithArguments", () -> trx.invokeWithArguments("s"));
        MethodHandle trs = tr.asType(MethodType.methodType(String.class, Object.class));
        drive("20 asType to interface invokeExact", () -> (String) trs.invokeExact((Object) "s"));
        MethodHandle invTrx = MethodHandles.invoker(trx.type());
        drive("21 plain invoker over explicitCast", () -> (String) invTrx.invokeExact(trx, (Object) "s"));

        // A plain invoker's conversions, both directions.
        MethodHandle invBoxedParam = MethodHandles.invoker(MethodType.methodType(int.class, Integer.class, int.class));
        drive("22 plain Integer parameter unboxed", () -> (int) invBoxedParam.invokeExact(s, (Integer) 5, 2));
        MethodHandle bx = l.findStatic(me, "boxed", MethodType.methodType(Integer.class, int.class));
        MethodHandle invIntInt = MethodHandles.invoker(MethodType.methodType(int.class, int.class));
        drive("23 plain Integer result unboxed", () -> (int) invIntInt.invokeExact(bx, 9));
        MethodHandle vv = l.findStatic(me, "sink", MethodType.methodType(void.class, int.class));
        MethodHandle invVoid = MethodHandles.invoker(MethodType.methodType(Object.class, int.class));
        drive("24 plain void result at Object", () -> (Object) invVoid.invokeExact(vv, 9));
        MethodHandle invObj1 = MethodHandles.invoker(MethodType.methodType(Object.class, Object.class));
        drive("25 plain array parameter cast", () -> (Object) invObj1.invokeExact(fst, (Object) "x"));

        // The other doors and an adapter over an exact invoker.
        drive("26 exact invokeWithArguments", () -> ex.invokeWithArguments(fst, arr));
        drive("27 exact invokeWithArguments target of another type", () -> ex.invokeWithArguments(fstS, arr));
        drive("28 spread converts its target", () -> sp0.invoke(fstS, new Object[] {arr}));
        MethodHandle folded = MethodHandles.foldArguments(ex,
                MethodHandles.dropArguments(MethodHandles.constant(MethodHandle.class, fstS), 0, Object[].class));
        drive("29 fold on exactInvoker, target of another type", () -> (Object) folded.invokeExact(arr));

        // A `findSpecial` handle's receiver is cast to its `specialCaller`
        // beneath an adapter too (the SPECIAL leaf cast to the method's class).
        MethodHandle sp = Sub.lookup().findSpecial(Mid.class, "m", MethodType.methodType(int.class), Sub.class);
        MethodHandle spx = MethodHandles.dropArguments(
                MethodHandles.explicitCastArguments(sp, MethodType.methodType(int.class, Object.class)), 0, int.class);
        drive("30 special under adapter, receiver not specialCaller", () -> (int) spx.invoke(1, (Object) new Mid()));
        drive("31 special under adapter, specialCaller receiver", () -> (int) spx.invoke(1, (Object) new Sub()));

        // `asType` casts a reference RETURN to an interface too; only an
        // `explicitCastArguments` stamp does not.
        MethodHandle ob = l.findStatic(me, "obj", MethodType.methodType(Object.class));
        drive("32 invoke return to interface", () -> {
            Runnable r = (Runnable) ob.invoke();
            return String.valueOf(r);
        });
        MethodHandle obr = ob.asType(MethodType.methodType(Runnable.class));
        drive("33 asType return to interface", () -> {
            Runnable r = (Runnable) obr.invokeExact();
            return String.valueOf(r);
        });
        MethodHandle obx = MethodHandles.explicitCastArguments(ob, MethodType.methodType(Runnable.class));
        drive("34 explicitCast return to interface", () -> {
            Runnable r = (Runnable) obx.invokeExact();
            return String.valueOf(r);
        });
        drive("35 explicitCast return to interface, invoke", () -> {
            Runnable r = (Runnable) obx.invoke();
            return String.valueOf(r);
        });
        MethodHandle idIns = MethodHandles.insertArguments(MethodHandles.identity(Object.class), 0, "t");
        drive("36 adapter invoke return to interface", () -> {
            Runnable r = (Runnable) idIns.invoke();
            return String.valueOf(r);
        });

        // An invoker casts its leading argument to MethodHandle.
        drive("37 invoker non-handle target", () -> (Object) inv.invoke((Object) "x", arr));
        drive("38 exactInvoker invokeWithArguments non-handle", () -> ex.invokeWithArguments("x", arr));
        MethodHandle insNull = MethodHandles.insertArguments(inv, 0, (Object) null);
        once("39 insert null target", () -> (Object) insNull.invoke(arr));
    }

    static Object obj() {
        return "s";
    }

    static class Grand {
        int m() {
            return 1;
        }
    }

    static class Mid extends Grand {
        @Override
        int m() {
            return 2;
        }
    }

    static class Sub extends Mid {
        @Override
        int m() {
            return 3;
        }

        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }
}
