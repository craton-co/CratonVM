// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-compatible-a-reflective-served-method-whose-callee-built-the-throwable-lists-no-jdk-frames-FIXED-20261026.md:
// a static JDK method `--compatible` serves with a native, reached by
// `Method.invoke`, whose throwable a CALLEE builds while the native runs (the
// served loader's `ClassNotFoundException`, a failed `<clinit>`'s
// `ExceptionInInitializerError`). HotSpot lists the method's JDK frames
// (`Class.forName` -> `forName0`) between the accessor and the raise. Each row
// prints the cause's class and frames up to the probe's own lambda, then the
// `InvocationTargetException`'s own top frames (the wrapper must not take the
// served method's frames). No lines. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// reflect-forName3: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$0
// reflect-forName3 wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$0
// reflect-forName3-init: java.lang.ExceptionInInitializerError java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$1
// reflect-forName3-init wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$1
// reflect-forName3-init-again: java.lang.NoClassDefFoundError java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$2
// reflect-forName3-init-again wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$2
// reflect-forName3-ok: class L3W62ReflectiveCalleeBuiltRaise$Fine
// direct-forName3: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W62ReflectiveCalleeBuiltRaise.lambda$main$4
// reflect-target-throws: java.lang.IllegalStateException L3W62ReflectiveCalleeBuiltRaise.boom | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$5
// reflect-target-throws wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$5
// reflect-forName3-again: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$6
// reflect-forName3-again wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62ReflectiveCalleeBuiltRaise.lambda$main$6
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W62ReflectiveCalleeBuiltRaise`.
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W62ReflectiveCalleeBuiltRaise {
    interface Row {
        Object run() throws Throwable;
    }

    static class Bad {
        static {
            if (true) {
                throw new IllegalStateException("bad");
            }
        }
    }

    static class Fine {
    }

    static String frames(Throwable t) {
        StringBuilder b = new StringBuilder(t.getClass().getName() + " ");
        String sep = "";
        for (StackTraceElement e : t.getStackTrace()) {
            b.append(sep).append(e.getClassName()).append('.').append(e.getMethodName())
                    .append(e.isNativeMethod() ? "(N)" : "");
            sep = " | ";
            if (e.getMethodName().startsWith("lambda$")) {
                break;
            }
        }
        return b.toString();
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            Throwable c = t instanceof InvocationTargetException ? t.getCause() : t;
            System.out.println(name + ": " + frames(c));
            if (c != t) {
                System.out.println(name + " wrapper: " + frames(t));
            }
        }
    }

    static void boom() {
        throw new IllegalStateException("boom");
    }

    public static void main(String[] args) throws Throwable {
        Method forName3 = Class.class.getMethod("forName", String.class, boolean.class, ClassLoader.class);
        Method boom = L3W62ReflectiveCalleeBuiltRaise.class.getDeclaredMethod("boom");
        ClassLoader app = L3W62ReflectiveCalleeBuiltRaise.class.getClassLoader();
        String bad = L3W62ReflectiveCalleeBuiltRaise.class.getName() + "$Bad";
        String fine = L3W62ReflectiveCalleeBuiltRaise.class.getName() + "$Fine";

        row("reflect-forName3", () -> forName3.invoke(null, "no.Such", false, app));
        row("reflect-forName3-init", () -> forName3.invoke(null, bad, true, app));
        row("reflect-forName3-init-again", () -> forName3.invoke(null, bad, true, app));
        row("reflect-forName3-ok", () -> forName3.invoke(null, fine, true, app));
        row("direct-forName3", () -> Class.forName("no.Such", false, app));
        row("reflect-target-throws", () -> boom.invoke(null));
        row("reflect-forName3-again", () -> forName3.invoke(null, "no.Such", false, app));
    }
}
