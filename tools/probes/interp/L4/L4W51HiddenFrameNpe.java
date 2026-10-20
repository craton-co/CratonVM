// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L4: a `NullPointerException` raised in a
// HIDDEN class's frame. HotSpot leaves a hidden frame out of the backtrace,
// so (1) the NPE has no extended (JEP 358) message, and (2) a reflective
// `Method.invoke` of the hidden method sees an NPE whose top frame is
// `DirectMethodHandleAccessor.invoke` itself, which its `isIllegalArgument`
// reads as a bad argument: `IllegalArgumentException`, not
// `InvocationTargetException`. Page
// `docs/internal/fixed-bugs/interpreter-L4-an-npe-in-a-hidden-frame-is-reported-as-the-frames-own-FIXED-20261016.md`.
// The direct rows (`plain-*`) are the visible-class control.
//
// Run: javac -d out L4W51HiddenFrameNpe.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W51HiddenFrameNpe
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   hidden-direct: java.lang.NullPointerException
//   hidden-reflective: java.lang.IllegalArgumentException: java.lang.NullPointerException
//   plain-direct: java.lang.NullPointerException: Cannot invoke "Object.hashCode()" because "<parameter1>" is null
//   plain-reflective: java.lang.reflect.InvocationTargetException / java.lang.NullPointerException: Cannot invoke "Object.hashCode()" because "<parameter1>" is null
//   hidden-direct-again: java.lang.NullPointerException
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W51HiddenFrameNpe {
    public static class H {
        public static int deref(Object o) {
            return o.hashCode();
        }
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (InvocationTargetException e) {
            out = e.getClass().getName() + " / " + e.getCause();
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        byte[] b;
        try (InputStream in = L4W51HiddenFrameNpe.class.getResourceAsStream("L4W51HiddenFrameNpe$H.class")) {
            b = in.readAllBytes();
        }
        Class<?> hidden = MethodHandles.lookup().defineHiddenClass(b, true).lookupClass();
        MethodHandle direct = MethodHandles.lookup().findStatic(hidden, "deref",
            MethodType.methodType(int.class, Object.class));
        Method reflective = hidden.getMethod("deref", Object.class);
        Method plain = H.class.getMethod("deref", Object.class);
        row("hidden-direct", () -> (int) direct.invokeExact((Object) null));
        row("hidden-reflective", () -> reflective.invoke(null, (Object) null));
        row("plain-direct", () -> H.deref(null));
        row("plain-reflective", () -> plain.invoke(null, (Object) null));
        row("hidden-direct-again", () -> (int) direct.invokeExact((Object) null));
    }
}
