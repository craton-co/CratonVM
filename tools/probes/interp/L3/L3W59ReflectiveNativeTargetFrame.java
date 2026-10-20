// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L3 (review) --
// docs/internal/fixed-bugs/interpreter-L3-a-native-target-reached-by-reflection-lists-no-native-frame-FIXED-20261024.md:
// a NATIVE method reached through `Method.invoke` or a method handle raises;
// HotSpot's trace starts with the native's own `(Native Method)` frame, as it
// does for a direct call (the `direct` row, which CratonVM matches). Rows
// print the innermost two frames. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// reflect: java.lang.System.arraycopy(Native Method) | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// handle: java.lang.System.arraycopy(Native Method) | L3W59ReflectiveNativeTargetFrame.lambda$main$1
// direct: java.lang.System.arraycopy(Native Method) | L3W59ReflectiveNativeTargetFrame.lambda$main$2
// reflect-again: java.lang.System.arraycopy(Native Method) | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W59ReflectiveNativeTargetFrame`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W59ReflectiveNativeTargetFrame {
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

    public static void main(String[] args) throws Throwable {
        Method ac = System.class.getMethod("arraycopy", Object.class, int.class, Object.class, int.class, int.class);
        MethodHandle mh = MethodHandles.lookup().findStatic(System.class, "arraycopy",
                MethodType.methodType(void.class, Object.class, int.class, Object.class, int.class, int.class));
        int[] src = new int[3];
        row("reflect", () -> ac.invoke(null, src, 0, "x", 0, 3));
        row("handle", () -> {
            mh.invokeExact((Object) src, 0, (Object) "x", 0, 3);
        });
        row("direct", () -> System.arraycopy(src, 0, "x", 0, 3));
        row("reflect-again", () -> ac.invoke(null, src, 0, "x", 0, 3));
    }
}
