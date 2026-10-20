// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L3 -- the i45-L3 proposal's frame row
// (docs/internal/fixed-bugs/interpreter-i45-L3-proposal-jdk-only-reflection-runs-the-jdks-own-accessor-DONE-20261014.md):
// a `null` reaching a primitive parameter through a method handle's
// `asType` unboxing raises its `NullPointerException` inside
// `sun.invoke.util.ValueConversions.unbox<Wrapper>(Object, boolean)`;
// HotSpot lists that frame on top (the `LambdaForm` frames below it are
// hidden). Through `Method.invoke` / `Constructor.newInstance` the NPE is the
// `IllegalArgumentException`'s cause. Rows print the innermost two frames of
// the NPE, no line numbers. Run it with `CRATONVM_JDK_REFLECTIVE_INVOKE` 0
// and 1: the expected lines are the same. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// reflect-int: sun.invoke.util.ValueConversions.unboxInteger | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// reflect-long: sun.invoke.util.ValueConversions.unboxLong | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// reflect-boolean: sun.invoke.util.ValueConversions.unboxBoolean | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// reflect-char: sun.invoke.util.ValueConversions.unboxCharacter | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// reflect-double: sun.invoke.util.ValueConversions.unboxDouble | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// ctor-int: sun.invoke.util.ValueConversions.unboxInteger | jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance
// handle-invoke-int: sun.invoke.util.ValueConversions.unboxInteger | L3W60NullUnboxFrames.lambda$main$6
// handle-asType-int: sun.invoke.util.ValueConversions.unboxInteger | L3W60NullUnboxFrames.lambda$main$7
// reflect-int-again: sun.invoke.util.ValueConversions.unboxInteger | jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W60NullUnboxFrames`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W60NullUnboxFrames {
    interface Row {
        void run() throws Throwable;
    }

    public L3W60NullUnboxFrames(int x) {}

    public static int takesInt(int x) {
        return x;
    }

    public static long takesLong(long x) {
        return x;
    }

    public static boolean takesBoolean(boolean x) {
        return x;
    }

    public static char takesChar(char x) {
        return x;
    }

    public static double takesDouble(double x) {
        return x;
    }

    static String frame(StackTraceElement e) {
        return e.getClassName() + "." + e.getMethodName() + (e.isNativeMethod() ? "(Native Method)" : "");
    }

    static void row(String name, Row r) {
        try {
            r.run();
            System.out.println(name + ": no exception");
        } catch (Throwable t) {
            Throwable c = t;
            while ((c instanceof InvocationTargetException || c instanceof IllegalArgumentException)
                    && c.getCause() != null) {
                c = c.getCause();
            }
            StackTraceElement[] st = c.getStackTrace();
            System.out.println(name + ": " + (st.length > 0 ? frame(st[0]) : "-") + " | "
                    + (st.length > 1 ? frame(st[1]) : "-"));
        }
    }

    public static void main(String[] args) throws Throwable {
        Class<?> p = L3W60NullUnboxFrames.class;
        Method takesInt = p.getMethod("takesInt", int.class);
        Method takesLong = p.getMethod("takesLong", long.class);
        Method takesBoolean = p.getMethod("takesBoolean", boolean.class);
        Method takesChar = p.getMethod("takesChar", char.class);
        Method takesDouble = p.getMethod("takesDouble", double.class);
        Constructor<?> ctor = p.getConstructor(int.class);
        MethodHandle intH = MethodHandles.lookup().findStatic(p, "takesInt",
                MethodType.methodType(int.class, int.class));
        MethodHandle objH = intH.asType(MethodType.methodType(int.class, Object.class));
        row("reflect-int", () -> takesInt.invoke(null, (Object) null));
        row("reflect-long", () -> takesLong.invoke(null, (Object) null));
        row("reflect-boolean", () -> takesBoolean.invoke(null, (Object) null));
        row("reflect-char", () -> takesChar.invoke(null, (Object) null));
        row("reflect-double", () -> takesDouble.invoke(null, (Object) null));
        row("ctor-int", () -> ctor.newInstance((Object) null));
        row("handle-invoke-int", () -> {
            int r = (int) intH.invoke((Object) null);
        });
        row("handle-asType-int", () -> {
            int r = (int) objH.invokeExact((Object) null);
        });
        row("reflect-int-again", () -> takesInt.invoke(null, (Object) null));
    }
}
