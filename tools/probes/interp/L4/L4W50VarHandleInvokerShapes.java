// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L4: more shapes of
// `MethodHandles.varHandleInvoker` / `varHandleExactInvoker` (answered
// natively since wave 50; page
// `docs/internal/fixed-bugs/interpreter-L4-varhandle-invokers-fail-to-link-FIXED-20261014.md`):
// a read-modify-write mode, a static field, an array element, a null handle,
// a boxed invoker type, a hot loop (compiled doors; the plain invoker over an
// invoke-exact handle refuses every time), and the exact invoker's refusal
// first and again last. CratonVM before the fix of `null-handle`: a
// message-less `NullPointerException`.
//
// Run: javac -d out L4W50VarHandleInvokerShapes.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W50VarHandleInvokerShapes
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   exact-wrong: WrongMethodTypeException: handle's method type (VarHandle,L4W50VarHandleInvokerShapes)int but found (VarHandle,L4W50VarHandleInvokerShapes)Integer
//   cas: true 6
//   get-and-add: 6 9
//   exact-get-and-add: 9 10
//   static: 11
//   static-exact: 11
//   array: 3
//   array-exact-wrong: WrongMethodTypeException: handle's method type (VarHandle,int[],int)int but found (VarHandle,int[],int)long
//   null-handle: NullPointerException: Cannot invoke "java.lang.invoke.VarHandle.asDirect()" because "handle" is null
//   boxed: 10
//   hot: 400000 20000
//   exact-wrong-again: WrongMethodTypeException: handle's method type (VarHandle,L4W50VarHandleInvokerShapes)int but found (VarHandle,L4W50VarHandleInvokerShapes)Integer
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.invoke.VarHandle.AccessMode;

public class L4W50VarHandleInvokerShapes {
    int i = 5;
    static int s = 11;

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getSimpleName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    static VarHandle vh;
    static L4W50VarHandleInvokerShapes t = new L4W50VarHandleInvokerShapes();
    static final Class<?> T = L4W50VarHandleInvokerShapes.class;

    static Object exactWrong() throws Throwable {
        MethodHandle mh = MethodHandles.varHandleExactInvoker(AccessMode.GET,
            MethodType.methodType(Integer.class, T));
        return (Integer) mh.invokeExact(vh, t);
    }

    public static void main(String[] args) throws Throwable {
        vh = MethodHandles.lookup().findVarHandle(T, "i", int.class);
        VarHandle svh = MethodHandles.lookup().findStaticVarHandle(T, "s", int.class);
        VarHandle avh = MethodHandles.arrayElementVarHandle(int[].class);
        int[] arr = {1, 2, 3};
        row("exact-wrong", L4W50VarHandleInvokerShapes::exactWrong);
        row("cas", () -> {
            MethodHandle mh = MethodHandles.varHandleInvoker(AccessMode.COMPARE_AND_SET,
                MethodType.methodType(boolean.class, T, int.class, int.class));
            boolean ok = (boolean) mh.invokeExact(vh, t, 5, 6);
            return ok + " " + t.i;
        });
        row("get-and-add", () -> {
            MethodHandle mh = MethodHandles.varHandleInvoker(AccessMode.GET_AND_ADD,
                MethodType.methodType(int.class, T, int.class));
            int old = (int) mh.invokeExact(vh, t, 3);
            return old + " " + t.i;
        });
        row("exact-get-and-add", () -> {
            MethodHandle mh = MethodHandles.varHandleExactInvoker(AccessMode.GET_AND_ADD,
                MethodType.methodType(int.class, T, int.class));
            int old = (int) mh.invokeExact(vh, t, 1);
            return old + " " + t.i;
        });
        row("static", () -> {
            MethodHandle mh = MethodHandles.varHandleInvoker(AccessMode.GET, MethodType.methodType(int.class));
            return (int) mh.invokeExact(svh);
        });
        row("static-exact", () -> {
            MethodHandle mh = MethodHandles.varHandleExactInvoker(AccessMode.GET, MethodType.methodType(int.class));
            return (int) mh.invokeExact(svh);
        });
        row("array", () -> {
            MethodHandle mh = MethodHandles.varHandleInvoker(AccessMode.GET,
                MethodType.methodType(int.class, int[].class, int.class));
            return (int) mh.invokeExact(avh, arr, 2);
        });
        row("array-exact-wrong", () -> {
            MethodHandle mh = MethodHandles.varHandleExactInvoker(AccessMode.GET,
                MethodType.methodType(long.class, int[].class, int.class));
            return (long) mh.invokeExact(avh, arr, 2);
        });
        row("null-handle", () -> {
            MethodHandle mh = MethodHandles.varHandleInvoker(AccessMode.GET, MethodType.methodType(int.class, T));
            return (int) mh.invokeExact((VarHandle) null, t);
        });
        row("boxed", () -> {
            MethodHandle mh = MethodHandles.varHandleInvoker(AccessMode.GET, MethodType.methodType(Integer.class, T));
            return (Integer) mh.invokeExact(vh, t);
        });
        row("hot", () -> {
            VarHandle exact = vh.withInvokeExactBehavior();
            MethodHandle plain = MethodHandles.varHandleInvoker(AccessMode.GET, MethodType.methodType(long.class, T));
            MethodHandle ex = MethodHandles.varHandleExactInvoker(AccessMode.GET, MethodType.methodType(int.class, T));
            long sum = 0;
            int wmte = 0;
            for (int k = 0; k < 20000; k++) {
                sum += (long) plain.invokeExact(vh, t);
                sum += (int) ex.invokeExact(vh, t);
                try {
                    sum += (long) plain.invokeExact(exact, t);
                } catch (java.lang.invoke.WrongMethodTypeException e) {
                    wmte++;
                }
            }
            return sum + " " + wmte;
        });
        row("exact-wrong-again", L4W50VarHandleInvokerShapes::exactWrong);
    }
}
