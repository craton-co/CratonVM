// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-a-native-target-reached-by-reflection-lists-no-native-frame-FIXED-20261024.md:
// a NATIVE method reached through `Method.invoke`, `MethodHandle.invokeExact`
// or `MethodHandle.invoke` raises; HotSpot's trace starts with the native's
// own `(Native Method)` frame, as for a direct call. Static (`arraycopy`,
// `Array.getLength`) and final instance (`Object.notify`) natives; a Java
// target as the control (its own frame on top, unchanged). Rows print the
// innermost two frames. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// reflect-arraycopy: java.lang.System.arraycopy(Native Method) | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// handle-arraycopy: java.lang.System.arraycopy(Native Method) | L3W60ReflectiveNativeTargetFrame.lambda$main$1
// generic-arraycopy: java.lang.System.arraycopy(Native Method) | L3W60ReflectiveNativeTargetFrame.lambda$main$2
// direct-arraycopy: java.lang.System.arraycopy(Native Method) | L3W60ReflectiveNativeTargetFrame.lambda$main$3
// reflect-notify: java.lang.Object.notify(Native Method) | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// handle-notify: java.lang.Object.notify(Native Method) | L3W60ReflectiveNativeTargetFrame.lambda$main$5
// direct-notify: java.lang.Object.notify(Native Method) | L3W60ReflectiveNativeTargetFrame.lambda$main$6
// reflect-getLength: java.lang.reflect.Array.getLength(Native Method) | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// handle-getLength: java.lang.reflect.Array.getLength(Native Method) | L3W60ReflectiveNativeTargetFrame.lambda$main$8
// direct-getLength: java.lang.reflect.Array.getLength(Native Method) | L3W60ReflectiveNativeTargetFrame.lambda$main$9
// reflect-java: L3W60ReflectiveNativeTargetFrame.thrower | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// handle-java: L3W60ReflectiveNativeTargetFrame.thrower | L3W60ReflectiveNativeTargetFrame.lambda$main$11
// reflect-arraycopy-again: java.lang.System.arraycopy(Native Method) | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W60ReflectiveNativeTargetFrame`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W60ReflectiveNativeTargetFrame {
    interface Row {
        void run() throws Throwable;
    }

    static String frame(StackTraceElement e) {
        // No line numbers: a JDK frame's moves between JDK updates.
        return e.getClassName() + "." + e.getMethodName() + (e.isNativeMethod() ? "(Native Method)" : "");
    }

    static void row(String name, Row r) {
        try {
            r.run();
            System.out.println(name + ": no exception");
        } catch (Throwable t) {
            Throwable c = t instanceof InvocationTargetException ? t.getCause() : t;
            StackTraceElement[] st = c.getStackTrace();
            System.out.println(name + ": " + (st.length > 0 ? frame(st[0]) : "-") + " | "
                    + (st.length > 1 ? frame(st[1]) : "-"));
        }
    }

    public static void thrower() {
        throw new IllegalStateException("java target");
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        Method ac = System.class.getMethod("arraycopy", Object.class, int.class, Object.class, int.class, int.class);
        MethodHandle acH = lookup.findStatic(System.class, "arraycopy",
                MethodType.methodType(void.class, Object.class, int.class, Object.class, int.class, int.class));
        Method notify = Object.class.getMethod("notify");
        MethodHandle notifyH = lookup.findVirtual(Object.class, "notify", MethodType.methodType(void.class));
        Method getLength = Array.class.getMethod("getLength", Object.class);
        MethodHandle getLengthH = lookup.findStatic(Array.class, "getLength",
                MethodType.methodType(int.class, Object.class));
        Method thrower = L3W60ReflectiveNativeTargetFrame.class.getMethod("thrower");
        MethodHandle throwerH = lookup.findStatic(L3W60ReflectiveNativeTargetFrame.class, "thrower",
                MethodType.methodType(void.class));
        int[] src = new int[3];
        Object lock = new Object();
        row("reflect-arraycopy", () -> ac.invoke(null, src, 0, "x", 0, 3));
        row("handle-arraycopy", () -> {
            acH.invokeExact((Object) src, 0, (Object) "x", 0, 3);
        });
        row("generic-arraycopy", () -> {
            acH.invoke(src, 0, "x", 0, 3);
        });
        row("direct-arraycopy", () -> System.arraycopy(src, 0, "x", 0, 3));
        row("reflect-notify", () -> notify.invoke(lock));
        row("handle-notify", () -> {
            notifyH.invokeExact(lock);
        });
        row("direct-notify", () -> lock.notify());
        row("reflect-getLength", () -> getLength.invoke(null, "x"));
        row("handle-getLength", () -> {
            int n = (int) getLengthH.invokeExact((Object) "x");
        });
        row("direct-getLength", () -> Array.getLength("x"));
        row("reflect-java", () -> thrower.invoke(null));
        row("handle-java", () -> {
            throwerH.invokeExact();
        });
        row("reflect-arraycopy-again", () -> ac.invoke(null, src, 0, "x", 0, 3));
    }
}
