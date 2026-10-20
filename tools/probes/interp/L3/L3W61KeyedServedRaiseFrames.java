// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-compatible-a-served-jdk-method-reached-by-reflection-lists-no-jdk-frames-FIXED-20261025.md
// and the proposal i60-L3-proposal-one-keyed-entry-for-the-served-raise-families:
// a JDK method `--compatible` serves with a native raises when reached with
// no call site naming it (`Method.invoke`, a method handle); HotSpot lists
// the method's JDK frames above the reflective frames. A served
// `setAccessible(true)` refusal lists the JDK's check chain. A static
// caller-sensitive method reached reflectively runs its
// `@CallerSensitiveAdapter` overload (one `Class.forName` frame). Rows print
// the throwable's class and its frames, no lines, up to the probe's own
// lambda. The first row is repeated last (a cold-VM first call).
// `reflect-forName3` (`--compatible`): the served `forName(String, boolean,
// ClassLoader)` raises a throwable its loader built, which no mark keys;
// matched since wave 62 by the reflective row's key
// (`CRATONVM_THROWABLE_SERVED_RAISE_ROW_KEY`,
// docs/internal/fixed-bugs/interpreter-L3-compatible-a-reflective-served-method-whose-callee-built-the-throwable-lists-no-jdk-frames-FIXED-20261026.md).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// reflect-wait: java.lang.IllegalMonitorStateException java.lang.Object.wait0(N) | java.lang.Object.wait | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W61KeyedServedRaiseFrames.lambda$main$0
// reflect-wait-negative: java.lang.IllegalArgumentException java.lang.Object.wait | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W61KeyedServedRaiseFrames.lambda$main$1
// reflect-forName: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W61KeyedServedRaiseFrames.lambda$main$2
// reflect-forName3: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W61KeyedServedRaiseFrames.lambda$main$3
// reflect-newArray: java.lang.NegativeArraySizeException java.lang.reflect.Array.newArray(N) | java.lang.reflect.Array.newInstance | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W61KeyedServedRaiseFrames.lambda$main$4
// reflect-newArray-void: java.lang.IllegalArgumentException java.lang.reflect.Array.newArray(N) | java.lang.reflect.Array.newInstance | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W61KeyedServedRaiseFrames.lambda$main$5
// reflect-getMethod: java.lang.NoSuchMethodException java.lang.Class.getMethod | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W61KeyedServedRaiseFrames.lambda$main$6
// handle-wait: java.lang.IllegalMonitorStateException java.lang.Object.wait0(N) | java.lang.Object.wait | L3W61KeyedServedRaiseFrames.lambda$main$7
// handle-newArray: java.lang.NegativeArraySizeException java.lang.reflect.Array.newArray(N) | java.lang.reflect.Array.newInstance | L3W61KeyedServedRaiseFrames.lambda$main$8
// direct-wait: java.lang.IllegalMonitorStateException java.lang.Object.wait0(N) | java.lang.Object.wait | L3W61KeyedServedRaiseFrames.lambda$main$9
// direct-forName: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | java.lang.Class.forName | L3W61KeyedServedRaiseFrames.lambda$main$10
// direct-setAccessible: java.lang.reflect.InaccessibleObjectException java.lang.reflect.AccessibleObject.throwInaccessibleObjectException | java.lang.reflect.AccessibleObject.checkCanSetAccessible | java.lang.reflect.AccessibleObject.checkCanSetAccessible | java.lang.reflect.Method.checkCanSetAccessible | java.lang.reflect.Method.setAccessible | L3W61KeyedServedRaiseFrames.lambda$main$11
// field-setAccessible: java.lang.reflect.InaccessibleObjectException java.lang.reflect.AccessibleObject.throwInaccessibleObjectException | java.lang.reflect.AccessibleObject.checkCanSetAccessible | java.lang.reflect.AccessibleObject.checkCanSetAccessible | java.lang.reflect.Field.checkCanSetAccessible | java.lang.reflect.Field.setAccessible | L3W61KeyedServedRaiseFrames.lambda$main$12
// ctor-setAccessible: java.lang.reflect.InaccessibleObjectException java.lang.reflect.AccessibleObject.throwInaccessibleObjectException | java.lang.reflect.AccessibleObject.checkCanSetAccessible | java.lang.reflect.AccessibleObject.checkCanSetAccessible | java.lang.reflect.Constructor.checkCanSetAccessible | java.lang.reflect.Constructor.setAccessible | L3W61KeyedServedRaiseFrames.lambda$main$13
// reflect-forName-again: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W61KeyedServedRaiseFrames.lambda$main$14
// reflect-wait-again: java.lang.IllegalMonitorStateException java.lang.Object.wait0(N) | java.lang.Object.wait | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W61KeyedServedRaiseFrames.lambda$main$15
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W61KeyedServedRaiseFrames`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

public class L3W61KeyedServedRaiseFrames {
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
        Method forName3 = Class.class.getMethod("forName", String.class, boolean.class, ClassLoader.class);
        Method newArray = Array.class.getMethod("newInstance", Class.class, int.class);
        Method getMethod = Class.class.getMethod("getMethod", String.class, Class[].class);
        Method clone = Object.class.getDeclaredMethod("clone");
        java.lang.reflect.Field value = String.class.getDeclaredField("value");
        Constructor<?> hidden = null;
        for (Constructor<?> c : String.class.getDeclaredConstructors()) {
            if (!Modifier.isPublic(c.getModifiers())) {
                hidden = c;
                break;
            }
        }
        Constructor<?> ctor = hidden;
        MethodHandle waitH = MethodHandles.lookup()
                .findVirtual(Object.class, "wait", MethodType.methodType(void.class, long.class));
        MethodHandle newArrayH = MethodHandles.lookup().findStatic(Array.class, "newInstance",
                MethodType.methodType(Object.class, Class.class, int.class));
        ClassLoader app = L3W61KeyedServedRaiseFrames.class.getClassLoader();

        row("reflect-wait", () -> waitM.invoke(lock, 10L));
        row("reflect-wait-negative", () -> waitM.invoke(lock, -1L));
        row("reflect-forName", () -> forName.invoke(null, "no.Such"));
        row("reflect-forName3", () -> forName3.invoke(null, "no.Such", false, app));
        row("reflect-newArray", () -> newArray.invoke(null, int.class, -1));
        row("reflect-newArray-void", () -> newArray.invoke(null, void.class, 1));
        row("reflect-getMethod", () -> getMethod.invoke(String.class, "noSuch", new Class<?>[0]));
        row("handle-wait", () -> { waitH.invokeExact(lock, 10L); });
        row("handle-newArray", () -> { Object o = (Object) newArrayH.invokeExact((Class<?>) int.class, -1); });
        row("direct-wait", () -> lock.wait(10L));
        row("direct-forName", () -> Class.forName("no.Such"));
        row("direct-setAccessible", () -> clone.setAccessible(true));
        row("field-setAccessible", () -> value.setAccessible(true));
        row("ctor-setAccessible", () -> ctor.setAccessible(true));
        row("reflect-forName-again", () -> forName.invoke(null, "no.Such"));
        row("reflect-wait-again", () -> waitM.invoke(lock, 10L));
    }
}
