// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L3 (review) --
// docs/internal/fixed-bugs/interpreter-L4-an-adapted-method-handle-passes-an-unchecked-reference-argument-FIXED-20261024.md:
// a direct method handle to a method taking `String[]` (or `String`), adapted
// to `Object` parameters with `asType` / `asSpreader` -- the shape JDK 25's
// `DirectMethodHandleAccessor` invokes for every reflective call -- must
// `checkcast` each reference argument. Measured as `L3W47ReflectiveArrayArgument`
// with `CRATONVM_JDK_REFLECTIVE_INVOKE=1`: the target ran with an `Integer[]`
// in its `String[]` parameter. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// astype-array: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// astype-string: java.lang.ClassCastException Cannot cast java.lang.Integer to java.lang.String
// spreader-array: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// generic-invoke-array: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// astype-ok: strings 1
// astype-array-again: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W59HandleReferenceArgumentCast`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L3W59HandleReferenceArgumentCast {
    public static String strings(String[] a) {
        return "strings " + a.length + " " + a.getClass().getName();
    }

    public static String one(String s) {
        return "one " + s.getClass().getName();
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            Object v = r.run();
            String s = String.valueOf(v);
            // `strings 1 [Ljava.lang.String;` -> `strings 1`: the class part is
            // the point of the failing rows, not of the passing one.
            System.out.println(name + ": " + (s.startsWith("strings 1 [Ljava.lang.String;") ? "strings 1" : s));
        } catch (Throwable t) {
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L3W59HandleReferenceArgumentCast.class;
        MethodHandle strings = l.findStatic(me, "strings", MethodType.methodType(String.class, String[].class));
        MethodHandle one = l.findStatic(me, "one", MethodType.methodType(String.class, String.class));
        MethodHandle g = strings.asType(MethodType.methodType(Object.class, Object.class));
        Object ints = new Integer[] {1};
        row("astype-array", () -> g.invokeExact(ints));
        MethodHandle g1 = one.asType(MethodType.methodType(Object.class, Object.class));
        Object i = Integer.valueOf(1);
        row("astype-string", () -> g1.invokeExact(i));
        MethodHandle spread = strings.asSpreader(Object[].class, 1)
                .asType(MethodType.methodType(Object.class, Object[].class));
        row("spreader-array", () -> spread.invokeExact(new Object[] {ints}));
        row("generic-invoke-array", () -> strings.invoke(ints));
        row("astype-ok", () -> g.invokeExact((Object) new String[] {"a"}));
        row("astype-array-again", () -> g.invokeExact(ints));
    }
}
