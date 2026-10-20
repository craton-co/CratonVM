// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 52, lane L4: an exception raised in a HIDDEN
// class's own frame under `Method.invoke` / `Constructor.newInstance`.
// HotSpot leaves the hidden frame out of the backtrace, so the exception's top
// frame is the reflective accessor's, and `AccessorUtils.isIllegalArgument`
// reads an NPE / CCE as a bad argument: `IllegalArgumentException`, not
// `InvocationTargetException`. A hidden method whose exception comes from a
// VISIBLE method it calls is wrapped as usual. Page
// `docs/internal/fixed-bugs/interpreter-L4-an-npe-in-a-hidden-frame-is-reported-as-the-frames-own-FIXED-20261016.md`.
// The first row is repeated last (a cold-VM-only gap shows as a differing row).
// The `hot-*` row runs a hidden method's null dereference through a method
// handle until it is compiled: every NPE must still have no message.
//
// Run: javac -d out L4W52HiddenReflectiveFailure.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W52HiddenReflectiveFailure
//
// Expected HotSpot 25 output (25.0.3, default flags):
//   hidden-npe: IAE: java.lang.NullPointerException / cause java.lang.NullPointerException
//   hidden-cce: IAE: argument type mismatch / cause null
//   hidden-calls-visible: ITE / cause java.lang.NullPointerException: Cannot invoke "Object.hashCode()" because "<parameter1>" is null
//   hidden-ctor-npe: IAE: java.lang.NullPointerException / cause java.lang.NullPointerException
//   hidden-ctor-cce: IAE: argument type mismatch / cause java.lang.ClassCastException
//   hidden-receiver: IAE: java.lang.NullPointerException / cause java.lang.NullPointerException
//   plain-npe: ITE / cause java.lang.NullPointerException: Cannot invoke "Object.hashCode()" because "<parameter1>" is null
//   hot-hidden-direct: messages=0 of 20000
//   hidden-npe-again: IAE: java.lang.NullPointerException / cause java.lang.NullPointerException
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.function.Supplier;

public class L4W52HiddenReflectiveFailure {
    public static class Vis {
        public static int deref(Object o) {
            return o.hashCode();
        }
    }

    public static class H {
        public H(Object o) {
            o.hashCode();
        }

        public H(Object o, int k) {
            String s = (String) o;
        }

        public static int deref(Object o) {
            return o.hashCode();
        }

        public static int cast(Object o) {
            return ((String) o).length();
        }

        public static int callsVisible(Object o) {
            return Vis.deref(o);
        }
    }

    public static class S implements Supplier<Object> {
        Object nil;

        public Object get() {
            return nil.hashCode();
        }
    }

    interface Body {
        Object run() throws Throwable;
    }

    static String name(Throwable t) {
        if (t instanceof IllegalArgumentException) return "IAE: " + t.getMessage();
        if (t instanceof InvocationTargetException) return "ITE";
        return String.valueOf(t);
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = "returned " + body.run();
        } catch (Throwable t) {
            Throwable c = t.getCause();
            String cause = c == null ? "null"
                : (t instanceof InvocationTargetException ? String.valueOf(c) : c.getClass().getName());
            out = name(t) + " / cause " + cause;
        }
        System.out.println(name + ": " + out);
    }

    static Class<?> hide(String file) throws Throwable {
        byte[] b;
        try (InputStream in = L4W52HiddenReflectiveFailure.class.getResourceAsStream(file)) {
            b = in.readAllBytes();
        }
        return MethodHandles.lookup().defineHiddenClass(b, true).lookupClass();
    }

    public static void main(String[] args) throws Throwable {
        Class<?> h = hide("L4W52HiddenReflectiveFailure$H.class");
        Class<?> s = hide("L4W52HiddenReflectiveFailure$S.class");
        Method deref = h.getMethod("deref", Object.class);
        Method cast = h.getMethod("cast", Object.class);
        Method callsVisible = h.getMethod("callsVisible", Object.class);
        Constructor<?> ctorNpe = h.getConstructor(Object.class);
        Constructor<?> ctorCce = h.getConstructor(Object.class, int.class);
        Object supplier = MethodHandles.lookup().findConstructor(s, MethodType.methodType(void.class)).invoke();
        Method get = Supplier.class.getMethod("get");
        Method plain = Vis.class.getMethod("deref", Object.class);

        row("hidden-npe", () -> deref.invoke(null, (Object) null));
        row("hidden-cce", () -> cast.invoke(null, (Object) Integer.valueOf(1)));
        row("hidden-calls-visible", () -> callsVisible.invoke(null, (Object) null));
        row("hidden-ctor-npe", () -> ctorNpe.newInstance((Object) null));
        row("hidden-ctor-cce", () -> ctorCce.newInstance(Integer.valueOf(1), 2));
        row("hidden-receiver", () -> get.invoke(supplier));
        row("plain-npe", () -> plain.invoke(null, (Object) null));

        MethodHandle direct = MethodHandles.lookup().findStatic(h, "deref",
            MethodType.methodType(int.class, Object.class));
        int messages = 0;
        int n = 20000;
        for (int i = 0; i < n; i++) {
            try {
                int r = (int) direct.invokeExact((Object) null);
            } catch (NullPointerException e) {
                if (e.getMessage() != null) messages++;
            }
        }
        System.out.println("hot-hidden-direct: messages=" + messages + " of " + n);

        row("hidden-npe-again", () -> deref.invoke(null, (Object) null));
    }
}
