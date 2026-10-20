// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 52, lane L4: an UNBOUND instance-method reference
// (`String::length`, an interface method, a user method) applied to a null
// receiver. HotSpot's spun lambda class does `invokevirtual` /
// `invokeinterface` on the null itself: a NullPointerException raised in the
// hidden proxy frame, so it has no message and its top visible frame is the
// caller, not the target method. CratonVM ran the target's body with a null
// `this` (`Cannot read field "value" because "this" is null`, top frame
// `String.length`). The first row is repeated last; the `hot` row runs until
// the call sites are compiled.
//
// Run: javac -d out L4W52LambdaNullReceiver.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W52LambdaNullReceiver
//
// Expected HotSpot 25 output (25.0.3, default flags):
//   string-length: java.lang.NullPointerException top=apply-site
//   iface-method: java.lang.NullPointerException top=apply-site
//   user-method: java.lang.NullPointerException top=apply-site
//   bifunction: java.lang.NullPointerException top=apply-site
//   hot: npes=20000 messages=0
//   string-length-again: java.lang.NullPointerException
import java.util.function.BiFunction;
import java.util.function.Consumer;
import java.util.function.Function;

public class L4W52LambdaNullReceiver {
    public static class User {
        public String hello(int n) {
            return "hello" + n;
        }
    }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        if (st.length == 0) return "none";
        return st[0].getClassName().equals(L4W52LambdaNullReceiver.class.getName())
            && st[0].getMethodName().startsWith("apply") ? "apply-site"
            : st[0].getClassName() + "." + st[0].getMethodName();
    }

    static Object applyF(Function<Object, Object> f, Object x) {
        return f.apply(x);
    }

    static void applyC(Consumer<Runnable> c, Runnable r) {
        c.accept(r);
    }

    static Object applyB(BiFunction<User, Integer, String> f, User u, int n) {
        return f.apply(u, n);
    }

    interface Body {
        void run() throws Throwable;
    }

    static void row(String name, Body b) {
        try {
            b.run();
            System.out.println(name + ": returned");
        } catch (Throwable t) {
            // After the hot loop HotSpot's C2 may throw its preallocated,
            // trace-less NPE (OmitStackTraceInFastThrow): no `top` there.
            String where = name.endsWith("-again") ? "" : " top=" + top(t);
            System.out.println(name + ": " + t + where);
        }
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) {
        Function<String, Integer> len = String::length;
        Function<Object, Object> lenRaw = (Function<Object, Object>) (Function<?, ?>) len;
        Consumer<Runnable> run = Runnable::run;
        Function<User, String> hello1 = u -> null;
        BiFunction<User, Integer, String> hello = User::hello;
        row("string-length", () -> applyF(lenRaw, null));
        row("iface-method", () -> applyC(run, null));
        row("user-method", () -> applyB(hello, null, 1));
        row("bifunction", () -> applyB(hello, null, 2));
        int npes = 0;
        int messages = 0;
        for (int i = 0; i < 20000; i++) {
            try {
                applyF(lenRaw, null);
            } catch (NullPointerException e) {
                npes++;
                if (e.getMessage() != null) messages++;
            }
        }
        System.out.println("hot: npes=" + npes + " messages=" + messages);
        row("string-length-again", () -> applyF(lenRaw, null));
    }
}
