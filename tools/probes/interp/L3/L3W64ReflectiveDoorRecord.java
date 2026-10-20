// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 64, lane L3 --
// docs/internal/fixed-bugs/interpreter-i63-L3-proposal-the-reflective-door-notes-the-door-record-DONE-20261104.md:
// the served `Method.invoke`'s door notes the thread's door record (the
// carrier every static JDK door writes under
// `CRATONVM_THROWABLE_SERVED_RAISE_DOOR_KEY`) instead of wave 62's row
// target, which is retired. With `DOOR_KEY=1` (and `ROW_KEY` unset) the
// `reflect-forName3*` rows must now list `Class.forName0(N) | Class.forName`
// between the loader frames and the accessor, as HotSpot does; `ROW_KEY=1`
// alone must answer the same (it now writes the same record). The wrapper
// rows (`InvocationTargetException`) keep HotSpot's two accessor frames: the
// record is restored before the native wraps. `reflect-in-handle` is the
// reflective door inside an application handle door (which notes nothing);
// `reflect-forName3-loop` repeats the door 300 times (with
// `CRATONVM_THROWABLE_SERVED_RAISE_DOOR_MEMO=1` all but the first take the
// memo) and prints the last trace. Controls: `reflect-target-throws` (an
// application target, never noted) and `reflect-forName3-ok`. `--jdk-only`
// must match with every switch state. Rows print the throwable's frames up
// to the probe's own lambda, no lines.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// reflect-forName3: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$0
// reflect-forName3 wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$0
// reflect-forName3-init: java.lang.ExceptionInInitializerError java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$1
// reflect-forName3-init wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$1
// reflect-forName3-init-again: java.lang.NoClassDefFoundError java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$2
// reflect-forName3-init-again wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$2
// reflect-forName3-ok: class L3W64ReflectiveDoorRecord$Fine
// reflect-in-handle: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.viaReflect | L3W64ReflectiveDoorRecord.lambda$main$4
// reflect-in-handle wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.viaReflect | L3W64ReflectiveDoorRecord.lambda$main$4
// reflect-target-throws: java.lang.IllegalStateException L3W64ReflectiveDoorRecord.boom | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$5
// reflect-target-throws wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$5
// reflect-forName3-loop: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$6
// reflect-forName3-loop wrapper: java.lang.reflect.InvocationTargetException jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W64ReflectiveDoorRecord.lambda$main$6
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W64ReflectiveDoorRecord`,
// CratonVM with `CRATONVM_THROWABLE_SERVED_RAISE_DOOR_KEY` unset / `=1`,
// `CRATONVM_THROWABLE_SERVED_RAISE_ROW_KEY` unset / `=1`, and
// `CRATONVM_THROWABLE_SERVED_RAISE_DOOR_MEMO` unset / `=1`. Expected under
// `--compatible`: every switch unset, the `reflect-forName3*`,
// `reflect-in-handle` and `-loop` cause rows lack `Class.forName0(N) |
// Class.forName` (wave 61's answer); with `DOOR_KEY=1` or `ROW_KEY=1` they
// match. Positive control: `CRATONVM_FRAME_TRACE=1` prints
// `[SERVED_RAISE_DOOR_KEY] enter forName(...) key=Some(..) ... memo=hit`
// for the loop's later doors with the memo on, and
// `[SERVED_RAISE_DOOR_KEY] door target ... keyed=true` for the CNFE rows.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W64ReflectiveDoorRecord {
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

    static Method forName3;
    static ClassLoader app;

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

    static Object viaReflect(String name) throws Throwable {
        return forName3.invoke(null, name, false, app);
    }

    public static void main(String[] args) throws Throwable {
        forName3 = Class.class.getMethod("forName", String.class, boolean.class, ClassLoader.class);
        app = L3W64ReflectiveDoorRecord.class.getClassLoader();
        Method boom = L3W64ReflectiveDoorRecord.class.getDeclaredMethod("boom");
        MethodHandle via = MethodHandles.lookup().findStatic(L3W64ReflectiveDoorRecord.class,
                "viaReflect", MethodType.methodType(Object.class, String.class));
        String bad = L3W64ReflectiveDoorRecord.class.getName() + "$Bad";
        String fine = L3W64ReflectiveDoorRecord.class.getName() + "$Fine";

        row("reflect-forName3", () -> forName3.invoke(null, "no.Such", false, app));
        row("reflect-forName3-init", () -> forName3.invoke(null, bad, true, app));
        row("reflect-forName3-init-again", () -> forName3.invoke(null, bad, true, app));
        row("reflect-forName3-ok", () -> forName3.invoke(null, fine, true, app));
        row("reflect-in-handle", () -> (Object) via.invokeExact("no.Such"));
        row("reflect-target-throws", () -> boom.invoke(null));
        row("reflect-forName3-loop", () -> {
            InvocationTargetException last = null;
            for (int i = 0; i < 300; i++) {
                try {
                    forName3.invoke(null, "no.Such" + (i % 3), false, app);
                } catch (InvocationTargetException e) {
                    last = e;
                }
            }
            throw last;
        });
    }
}
