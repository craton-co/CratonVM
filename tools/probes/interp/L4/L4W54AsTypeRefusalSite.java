// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L4: an `asType` arity refusal that the
// MethodHandle doors do not judge up front (the `tryFinally` and loop
// combinators, whose `type` the doors' own arity checks do not speak for)
// names the handle's type AND the call site's, as HotSpot's invoker does
// (proposal
// `docs/internal/fixed-bugs/interpreter-L4-proposal-one-astype-refusal-record-for-every-invoke-door-FIXED-20261018.md`).
// With the doors' adapter switches at their defaults the `tryFinally` and
// loop rows are refused at the door already (control rows); the
// `constant` / `identity` / array-accessor / `throwException` rows were not
// (code read, wave 53: `constant(int, 7).invoke(1)` answered 7, the arm
// ignores its arguments) -- `CRATONVM_MH_ASTYPE_REFUSAL_SITE_TEXT`.
//
// Positive control of the leaf-refusal rewording (the same switch): run with
// `CRATONVM_MH_INVOKE_ADAPTER_ARITY=0 CRATONVM_MH_LOOP_KINDS_TYPED=0
// CRATONVM_DBG_MH_DISPATCH=1`; the `finally-*-short` rows then reach the
// leaf (`... to a call with 0 argument(s)` without the switch) and must still
// print HotSpot's lines, with a `[MH_ASTYPE_SITE]` line on stderr for each.
//
// Run: javac -d out L4W54AsTypeRefusalSite.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W54AsTypeRefusalSite
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   constant-invoke-long-first: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()int to (int)int
//   finally-invoke-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int)int to ()int
//   finally-ok: 8
//   finally-iwa-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int)int to ()Object
//   finally-exact-short: java.lang.invoke.WrongMethodTypeException: handle's method type (int)int but found ()int
//   loop-invoke-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int)int to ()int
//   loop-ok: 3
//   loop-iwa-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int)int to ()Object
//   loop-exact-short: java.lang.invoke.WrongMethodTypeException: handle's method type (int)int but found ()int
//   constant-invoke-long: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()int to (int)int
//   constant-ok: 7
//   constant-iwa-long: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()int to (Object)Object
//   identity-invoke-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int)int to ()int
//   identity-invoke-long: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int)int to (int,int)int
//   identity-iwa-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int)int to ()Object
//   array-get-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int[],int)int to (int[])int
//   array-get-ok: 6
//   array-set-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int[],int,int)void to (int[],int)void
//   array-length-long: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int[])int to (int[],int)int
//   array-length-iwa-long: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int[])int to (Object,Object)Object
//   zero-invoke-long: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()long to (long)long
//   throw-invoke-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(IllegalStateException)int to ()int
//   throw-ok: java.lang.IllegalStateException: boom
//   finally-invoke-short-again: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int)int to ()int
//   constant-invoke-long-again: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()int to (int)int
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W54AsTypeRefusalSite {
    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    static int twice(int x) {
        return 2 * x;
    }

    static int cleanup(Throwable t, int result, int x) {
        return result;
    }

    static int step(int v, int i, int n) {
        return v + 1;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle seven = MethodHandles.constant(int.class, 7);
        row("constant-invoke-long-first", () -> (int) seven.invoke(1));
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W54AsTypeRefusalSite.class;
        MethodHandle target = l.findStatic(me, "twice", MethodType.methodType(int.class, int.class));
        MethodHandle cleanup = l.findStatic(me, "cleanup",
                MethodType.methodType(int.class, Throwable.class, int.class, int.class));
        MethodHandle fin = MethodHandles.tryFinally(target, cleanup);
        row("finally-invoke-short", () -> (int) fin.invoke());
        row("finally-ok", () -> (int) fin.invoke(4));
        row("finally-iwa-short", () -> fin.invokeWithArguments());
        row("finally-exact-short", () -> (int) fin.invokeExact());
        // countedLoop(iterations, init, body): (int)int, counting n times from 0.
        MethodHandle iterations = MethodHandles.identity(int.class);
        MethodHandle init = MethodHandles.dropArguments(MethodHandles.constant(int.class, 0), 0, int.class);
        MethodHandle body = l.findStatic(me, "step",
                MethodType.methodType(int.class, int.class, int.class, int.class));
        MethodHandle loop = MethodHandles.countedLoop(iterations, init, body);
        row("loop-invoke-short", () -> (int) loop.invoke());
        row("loop-ok", () -> (int) loop.invoke(3));
        row("loop-iwa-short", () -> loop.invokeWithArguments());
        row("loop-exact-short", () -> (int) loop.invokeExact());
        // The kinds the doors judged only for `invokeExact` (wave 40): a call
        // of another arity through `invoke` / `invokeWithArguments` is
        // `asType`'s refusal on HotSpot.
        MethodHandle same = MethodHandles.identity(int.class);
        MethodHandle get = MethodHandles.arrayElementGetter(int[].class);
        MethodHandle set = MethodHandles.arrayElementSetter(int[].class);
        MethodHandle len = MethodHandles.arrayLength(int[].class);
        MethodHandle zero = MethodHandles.zero(long.class);
        MethodHandle thrower = MethodHandles.throwException(int.class, IllegalStateException.class);
        int[] arr = {5, 6};
        row("constant-invoke-long", () -> (int) seven.invoke(1));
        row("constant-ok", () -> (int) seven.invoke());
        row("constant-iwa-long", () -> seven.invokeWithArguments(1));
        row("identity-invoke-short", () -> (int) same.invoke());
        row("identity-invoke-long", () -> (int) same.invoke(1, 2));
        row("identity-iwa-short", () -> same.invokeWithArguments());
        row("array-get-short", () -> (int) get.invoke(arr));
        row("array-get-ok", () -> (int) get.invoke(arr, 1));
        row("array-set-short", () -> {
            set.invoke(arr, 0);
            return "no-throw " + arr[0];
        });
        row("array-length-long", () -> (int) len.invoke(arr, 1));
        row("array-length-iwa-long", () -> len.invokeWithArguments(arr, 1));
        row("zero-invoke-long", () -> (long) zero.invoke(3L));
        row("throw-invoke-short", () -> (int) thrower.invoke());
        row("throw-ok", () -> (int) thrower.invoke(new IllegalStateException("boom")));
        row("finally-invoke-short-again", () -> (int) fin.invoke());
        row("constant-invoke-long-again", () -> (int) seven.invoke(1));
    }
}
