// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L4 (review; page
// `docs/internal/fixed-bugs/interpreter-L4-varhandle-invokers-fail-to-link-FIXED-20261014.md`):
// `MethodHandles.varHandleInvoker` / `varHandleExactInvoker`, the method
// handles that invoke an access mode on a `VarHandle` passed as their first
// argument. CratonVM builds them through the JDK's `Invokers` bytecode, which
// fails in `BoundMethodHandle`'s species initialisation
// (`InternalError: Failed to link speciesData to speciesCode:
// java.lang.invoke.SimpleMethodHandle`, then `NoClassDefFoundError: Could not
// initialize class java.lang.invoke.BoundMethodHandle` for every later row).
// Fixed in wave 50 (lane L4): both factories are answered natively.
//
// Run: javac -d out L4W49VarHandleInvokers.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W49VarHandleInvokers
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   invoker: 4
//   invoker-type: (VarHandle,L4W49VarHandleInvokers)int
//   exact-invoker: 4
//   exact-invoker-type: (VarHandle,L4W49VarHandleInvokers)int
//   invoker-widen: 4
//   exact-invoker-wrong: WrongMethodTypeException: handle's method type (VarHandle,L4W49VarHandleInvokers)int but found (VarHandle,L4W49VarHandleInvokers)long
//   invoker-set: 9
//   invoker-exact-handle-wrong: WrongMethodTypeException: handle's method type (L4W49VarHandleInvokers)int but found (L4W49VarHandleInvokers)long
//   invoker-again: 9
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W49VarHandleInvokers {
    int i = 4;

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getSimpleName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        VarHandle vh = MethodHandles.lookup().findVarHandle(L4W49VarHandleInvokers.class, "i", int.class);
        VarHandle exact = vh.withInvokeExactBehavior();
        L4W49VarHandleInvokers t = new L4W49VarHandleInvokers();
        MethodType getInt = MethodType.methodType(int.class, L4W49VarHandleInvokers.class);
        MethodType getLong = MethodType.methodType(long.class, L4W49VarHandleInvokers.class);
        row("invoker", () -> (int) MethodHandles.varHandleInvoker(VarHandle.AccessMode.GET, getInt).invokeExact(vh, t));
        row("invoker-type", () -> MethodHandles.varHandleInvoker(VarHandle.AccessMode.GET, getInt).type());
        row("exact-invoker", () -> (int) MethodHandles.varHandleExactInvoker(VarHandle.AccessMode.GET, getInt).invokeExact(vh, t));
        row("exact-invoker-type", () -> MethodHandles.varHandleExactInvoker(VarHandle.AccessMode.GET, getInt).type());
        row("invoker-widen", () -> (long) MethodHandles.varHandleInvoker(VarHandle.AccessMode.GET, getLong).invokeExact(vh, t));
        row("exact-invoker-wrong", () -> (long) MethodHandles.varHandleExactInvoker(VarHandle.AccessMode.GET, getLong).invokeExact(vh, t));
        row("invoker-set", () -> {
            MethodHandle set = MethodHandles.varHandleInvoker(VarHandle.AccessMode.SET,
                MethodType.methodType(void.class, L4W49VarHandleInvokers.class, int.class));
            set.invokeExact(vh, t, 9);
            return t.i;
        });
        row("invoker-exact-handle-wrong", () -> (long) MethodHandles.varHandleInvoker(VarHandle.AccessMode.GET, getLong).invokeExact(exact, t));
        row("invoker-again", () -> (int) MethodHandles.varHandleInvoker(VarHandle.AccessMode.GET, getInt).invokeExact(vh, t));
    }
}
