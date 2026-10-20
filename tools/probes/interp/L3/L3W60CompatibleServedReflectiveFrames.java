// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L3 (review) --
// docs/internal/fixed-bugs/interpreter-L3-compatible-a-served-jdk-method-reached-by-reflection-lists-no-jdk-frames-FIXED-20261025.md:
// a JDK method `--compatible` serves with a native (`Object.wait(long)`,
// `Class.forName(String)`, `Array.newInstance`, `AccessibleObject.setAccessible`)
// raises; HotSpot lists the JDK frames of the method (and its native leaf),
// whether it was called directly or through `Method.invoke`. Rows print the
// throwable's class and its innermost frames, no line numbers, up to the
// probe's own lambda. `--jdk-only` (where the JDK's bodies run) matches but
// for `reflect-forName` under the served `Method.invoke` (an extra
// `Class.forName` frame: HotSpot's reflection calls the caller-sensitive
// adapter `forName(String, Class)` directly). The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// reflect-wait: java.lang.IllegalMonitorStateException java.lang.Object.wait0(N) | java.lang.Object.wait | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W60CompatibleServedReflectiveFrames.lambda$main$0
// direct-wait: java.lang.IllegalMonitorStateException java.lang.Object.wait0(N) | java.lang.Object.wait | L3W60CompatibleServedReflectiveFrames.lambda$main$1
// reflect-forName: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W60CompatibleServedReflectiveFrames.lambda$main$2
// direct-forName: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | java.lang.Class.forName | L3W60CompatibleServedReflectiveFrames.lambda$main$3
// reflect-newArray: java.lang.NegativeArraySizeException java.lang.reflect.Array.newArray(N) | java.lang.reflect.Array.newInstance | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W60CompatibleServedReflectiveFrames.lambda$main$4
// direct-newArray: java.lang.NegativeArraySizeException java.lang.reflect.Array.newArray(N) | java.lang.reflect.Array.newInstance | L3W60CompatibleServedReflectiveFrames.lambda$main$5
// direct-setAccessible: java.lang.reflect.InaccessibleObjectException java.lang.reflect.AccessibleObject.throwInaccessibleObjectException | java.lang.reflect.AccessibleObject.checkCanSetAccessible | java.lang.reflect.AccessibleObject.checkCanSetAccessible | java.lang.reflect.Method.checkCanSetAccessible | java.lang.reflect.Method.setAccessible | L3W60CompatibleServedReflectiveFrames.lambda$main$6
// reflect-wait-again: java.lang.IllegalMonitorStateException java.lang.Object.wait0(N) | java.lang.Object.wait | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W60CompatibleServedReflectiveFrames.lambda$main$7
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W60CompatibleServedReflectiveFrames`.
import java.lang.reflect.Array;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W60CompatibleServedReflectiveFrames {
    interface Row {
        void run() throws Throwable;
    }

    static String frame(StackTraceElement e) {
        return e.getClassName() + "." + e.getMethodName() + (e.isNativeMethod() ? "(N)" : "");
    }

    static void row(String name, Row r) {
        try {
            r.run();
            System.out.println(name + ": no exception");
        } catch (Throwable t) {
            Throwable c = t instanceof InvocationTargetException ? t.getCause() : t;
            StringBuilder b = new StringBuilder(name + ": " + c.getClass().getName() + " ");
            String sep = "";
            for (StackTraceElement e : c.getStackTrace()) {
                b.append(sep).append(frame(e));
                sep = " | ";
                if (e.getMethodName().startsWith("lambda$")) {
                    break;
                }
            }
            System.out.println(b);
        }
    }

    public static void main(String[] args) throws Throwable {
        Object lock = new Object();
        Method waitM = Object.class.getMethod("wait", long.class);
        Method forName = Class.class.getMethod("forName", String.class);
        Method newArray = Array.class.getMethod("newInstance", Class.class, int.class);
        Method clone = Object.class.getDeclaredMethod("clone");
        row("reflect-wait", () -> waitM.invoke(lock, 10L));
        row("direct-wait", () -> lock.wait(10L));
        row("reflect-forName", () -> forName.invoke(null, "no.Such"));
        row("direct-forName", () -> Class.forName("no.Such"));
        row("reflect-newArray", () -> newArray.invoke(null, int.class, -1));
        row("direct-newArray", () -> Array.newInstance(int.class, -1));
        row("direct-setAccessible", () -> clone.setAccessible(true));
        row("reflect-wait-again", () -> waitM.invoke(lock, 10L));
    }
}
