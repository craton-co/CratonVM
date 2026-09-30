// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 27, lane L4: the `type()` of an invoker handle
// (`MethodHandles.exactInvoker` / `invoker` / `spreadInvoker`) is the
// INVOKER's own -- the target handle leads, and a spread invoker's trailing
// parameters are one Object[] -- and adapters chained on an invoker
// (`foldArguments`, `insertArguments`, Groovy's shape) compute their type from
// it. Also the return conversions an invoker's `invoke` / `invokeWithArguments`
// make now that the handle records its target's descriptor. Page:
// docs/internal/fixed-bugs/interpreter-L4-an-invoker-handle-reports-its-targets-type-FIXED-20260928.md
//
// Each row runs WARM times in `drive` (the call site is in the row's lambda,
// so the JIT compiles it); a row prints its first outcome and, if any later
// iteration answered differently, how many did.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W27InvokerTypes
// (all four combinations must print exactly HotSpot's lines)
//
// HotSpot 25 (25.0.3, default and -Xint) prints, verbatim:
//   1 exactInvoker type: (MethodHandle,Object[])Object MethodType
//   2 invoker type: (MethodHandle,Object[])Object MethodType
//   3 spreadInvoker 0 type: (MethodHandle,Object[])String MethodType
//   4 spreadInvoker 1 type: (MethodHandle,String,Object[])String MethodType
//   5 invoker parameterCount: 3 Integer
//   6 exactInvoker invokeExact: a String
//   7 invoker invoke int: 3 Integer
//   7l invoker invoke widened to long: 3 Long
//   7w invoker invokeWithArguments: 7 Integer
//   8 spreadInvoker invoke: ab String
//   9 fold on exactInvoker type: (String)String MethodType
//   9i fold on exactInvoker invoke: ABC String
//   9x fold on exactInvoker invokeExact: ABD String
//   10 insert on invoker type: (String)String MethodType
//   10i insert on invoker invoke: XYZ String
//   11 drop on invoker type: (MethodHandle,String,int,int)int MethodType
//   11i drop on invoker invoke: 11 Integer
//
// CratonVM before the fix (read from the code, not run; both modes): rows 1-4
// printed the TARGET type (`(Object[])Object`, `(String,int)String`,
// `(String,String)String`), row 5 `2`; row 9's type lost the wrong parameter
// (`()String`), so 9x was a WrongMethodTypeException under the strict
// `invokeExact` check; row 10's bound handle was judged against the target's
// first parameter (`String`) and its type lost `String`; row 11's type was
// `(int,String,int)int`; rows 7l and 7w returned the target's raw `int` (the
// handle's `MH_DESC` was empty, so nothing boxed or widened it). Rows 6, 7 and
// 8 are controls.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W27InvokerTypes {
    static final int WARM = 2000;

    interface Body {
        Object run() throws Throwable;
    }

    static int sum(int a, int b) {
        return a + b;
    }

    static Object first(Object[] xs) {
        return xs.length == 0 ? "none" : xs[0];
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

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType t = MethodType.methodType(Object.class, Object[].class);
        MethodHandle cat = l.findVirtual(String.class, "concat", MethodType.methodType(String.class, String.class));
        MethodHandle upper = l.findVirtual(String.class, "toUpperCase", MethodType.methodType(String.class));
        MethodHandle s = l.findStatic(L4W27InvokerTypes.class, "sum",
                MethodType.methodType(int.class, int.class, int.class));
        MethodHandle fst = l.findStatic(L4W27InvokerTypes.class, "first", t);

        // The page's four rows.
        drive("1 exactInvoker type", () -> MethodHandles.exactInvoker(t).type());
        drive("2 invoker type", () -> MethodHandles.invoker(t).type());
        drive("3 spreadInvoker 0 type", () -> MethodHandles.spreadInvoker(
                MethodType.methodType(String.class, String.class, int.class), 0).type());
        drive("4 spreadInvoker 1 type", () -> MethodHandles.spreadInvoker(cat.type(), 1).type());
        drive("5 invoker parameterCount", () -> MethodHandles.invoker(s.type()).type().parameterCount());

        // Invoking them: the arguments after the target reach it as passed.
        MethodHandle ex = MethodHandles.exactInvoker(t);
        drive("6 exactInvoker invokeExact", () -> (Object) ex.invokeExact(fst, new Object[] {"a", "b"}));
        MethodHandle inv = MethodHandles.invoker(s.type());
        drive("7 invoker invoke int", () -> (int) inv.invoke(s, 1, 2));
        drive("7l invoker invoke widened to long", () -> (long) inv.invoke(s, 1, 2));
        drive("7w invoker invokeWithArguments", () -> inv.invokeWithArguments(s, 3, 4));
        MethodHandle si = MethodHandles.spreadInvoker(cat.type(), 1);
        drive("8 spreadInvoker invoke", () -> (String) si.invoke(cat, "a", new Object[] {"b"}));

        // Adapters chained on an invoker take its type.
        MethodHandle target = MethodHandles.dropArguments(
                MethodHandles.constant(MethodHandle.class, upper), 0, String.class);
        MethodHandle folded = MethodHandles.foldArguments(MethodHandles.exactInvoker(upper.type()), target);
        drive("9 fold on exactInvoker type", () -> folded.type());
        drive("9i fold on exactInvoker invoke", () -> (String) folded.invoke("abc"));
        drive("9x fold on exactInvoker invokeExact", () -> (String) folded.invokeExact("abd"));
        MethodHandle bound = MethodHandles.insertArguments(MethodHandles.invoker(upper.type()), 0, upper);
        drive("10 insert on invoker type", () -> bound.type());
        drive("10i insert on invoker invoke", () -> (String) bound.invoke("xyz"));
        MethodHandle dropped = MethodHandles.dropArguments(MethodHandles.invoker(s.type()), 1, String.class);
        drive("11 drop on invoker type", () -> dropped.type());
        drive("11i drop on invoker invoke", () -> (int) dropped.invoke(s, "skip", 5, 6));
    }
}
