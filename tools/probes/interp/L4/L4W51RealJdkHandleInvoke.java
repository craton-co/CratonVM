// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L4: a REAL JDK `MethodHandle` (one the
// JDK's own `java.lang.invoke` code builds, not CratonVM's synthetic layout)
// invoked through the public doors (page
// `docs/internal/fixed-bugs/interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020.md`).
// `MethodType.invokers().varHandleMethodInvoker(GET)` is the JDK body of
// `MethodHandles.varHandleInvoker`, reached reflectively so the registered
// native does not stand in front: a `MethodHandleImpl$WrappedMember` over a
// `DirectMethodHandle` of a `LambdaForm` linker method. CratonVM (wave 51):
// `invoke: 0`, `invoke-with-arguments: null`, silently.
//
// Run: javac -d out L4W51RealJdkHandleInvoke.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] \
//          --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W51RealJdkHandleInvoke
//   (HotSpot: java --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out ...)
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   class: java.lang.invoke.MethodHandleImpl$WrappedMember
//   type: (VarHandle,L4W51RealJdkHandleInvoke)int
//   invoke: 4
//   invoke-exact: 4
//   invoke-with-arguments: 4
//   invoke-again: 4
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Method;

public class L4W51RealJdkHandleInvoke {
    int i = 4;

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

    public static void main(String[] args) throws Throwable {
        VarHandle vh = MethodHandles.lookup().findVarHandle(L4W51RealJdkHandleInvoke.class, "i", int.class);
        MethodType t = MethodType.methodType(int.class, L4W51RealJdkHandleInvoke.class);
        Method invokers = MethodType.class.getDeclaredMethod("invokers");
        invokers.setAccessible(true);
        Object inv = invokers.invoke(t);
        Method m = inv.getClass().getDeclaredMethod("varHandleMethodInvoker", VarHandle.AccessMode.class);
        m.setAccessible(true);
        MethodHandle h = (MethodHandle) m.invoke(inv, VarHandle.AccessMode.GET);
        L4W51RealJdkHandleInvoke o = new L4W51RealJdkHandleInvoke();
        row("class", () -> h.getClass().getName());
        row("type", () -> h.type());
        row("invoke", () -> (int) h.invoke(vh, o));
        row("invoke-exact", () -> (int) h.invokeExact(vh, o));
        row("invoke-with-arguments", () -> h.invokeWithArguments(vh, o));
        row("invoke-again", () -> (int) h.invoke(vh, o));
    }
}
