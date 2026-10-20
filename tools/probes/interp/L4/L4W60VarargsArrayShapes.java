// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L4 (review of the array-cast fix,
// docs/internal/fixed-bugs/interpreter-L4-an-adapted-method-handle-passes-an-unchecked-reference-argument-FIXED-20261024.md):
// once every adapter door judges an array argument's component, the varargs
// shapes must still COLLECT where HotSpot collects (a trailing argument the
// call site does not type as an assignable array is an ELEMENT, cast to the
// component) and pass an exactly typed array through; spreaders, the spread
// invoker, `arrayLength` and `explicitCastArguments` cast the array itself.
// The first `va-object-site-strings` row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// va-typed: va String[]#1
// va-object-site-strings: java.lang.ClassCastException Cannot cast [Ljava.lang.String; to java.lang.String
// va-objects-array-site: java.lang.ClassCastException Cannot cast [Ljava.lang.Object; to java.lang.String
// va-two-strings: va String[]#2
// va-iwa-strings: java.lang.ClassCastException Cannot cast [Ljava.lang.String; to java.lang.String
// va-iwa-list: va String[]#2
// va-iwa-objects: java.lang.ClassCastException Cannot cast [Ljava.lang.Object; to java.lang.String
// va-iwa-nested: java.lang.ClassCastException Cannot cast [Ljava.lang.Object; to java.lang.String
// objs-iwa-strings: objs Object[]#1
// objs-invoke-strings: objs Object[]#1
// spread-strings: two ab
// spread-objects: java.lang.ClassCastException Cannot cast [Ljava.lang.Object; to [Ljava.lang.String;
// spread-o-objects: two ab
// spread-o-ints: java.lang.ClassCastException Cannot cast java.lang.Integer to java.lang.String
// spread-invoker-strings: two ab
// array-length-ints: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// array-length-strings: 2
// explicit-array: java.lang.ClassCastException Cannot cast [Ljava.lang.Object; to [Ljava.lang.String;
// va-again-object-site: java.lang.ClassCastException Cannot cast [Ljava.lang.String; to java.lang.String
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W60VarargsArrayShapes`.
import java.lang.invoke.*;
import java.util.*;

public class L4W60VarargsArrayShapes {
    public static String va(String... a) { return "va " + a.getClass().getSimpleName() + "#" + a.length; }
    public static String two(String x, String y) { return "two " + x + y; }
    public static String objs(Object... a) { return "objs " + a.getClass().getSimpleName() + "#" + a.length; }

    interface Row { Object run() throws Throwable; }
    static void row(String n, Row r) {
        try { System.out.println(n + ": " + r.run()); }
        catch (Throwable t) { System.out.println(n + ": " + t.getClass().getName() + " " + t.getMessage()); }
    }

    public static void main(String[] a) throws Throwable {
        var l = MethodHandles.lookup();
        MethodHandle va = l.findStatic(L4W60VarargsArrayShapes.class, "va", MethodType.methodType(String.class, String[].class));
        MethodHandle two = l.findStatic(L4W60VarargsArrayShapes.class, "two", MethodType.methodType(String.class, String.class, String.class));
        MethodHandle objs = l.findStatic(L4W60VarargsArrayShapes.class, "objs", MethodType.methodType(String.class, Object[].class));
        Object strs = new String[] {"a", "b"};
        Object objsArr = new Object[] {"a", "b"};
        row("va-typed", () -> (String) va.invoke(new String[] {"a"}));
        row("va-object-site-strings", () -> (Object) va.invoke(strs));
        row("va-objects-array-site", () -> (Object) va.invoke((Object[]) objsArr));
        row("va-two-strings", () -> (Object) va.invoke("a", "b"));
        row("va-iwa-strings", () -> va.invokeWithArguments(strs));
        row("va-iwa-list", () -> va.invokeWithArguments(List.of("a", "b")));
        row("va-iwa-objects", () -> va.invokeWithArguments(objsArr));
        row("va-iwa-nested", () -> va.invokeWithArguments(new Object[] {objsArr}));
        row("objs-iwa-strings", () -> objs.invokeWithArguments(new Object[] {strs}));
        row("objs-invoke-strings", () -> (Object) objs.invoke(strs));
        MethodHandle sp = two.asSpreader(String[].class, 2);
        row("spread-strings", () -> (Object) sp.invoke(strs));
        row("spread-objects", () -> (Object) sp.invoke(objsArr));
        MethodHandle spo = two.asSpreader(Object[].class, 2);
        row("spread-o-objects", () -> (Object) spo.invoke(objsArr));
        row("spread-o-ints", () -> (Object) spo.invoke((Object) new Object[] {1, 2}));
        MethodHandle inv = MethodHandles.spreadInvoker(two.type(), 0);
        row("spread-invoker-strings", () -> (Object) inv.invoke(two, objsArr));
        MethodHandle len = MethodHandles.arrayLength(String[].class);
        row("array-length-ints", () -> (Object) len.invoke((Object) new Integer[3]));
        row("array-length-strings", () -> (Object) len.invoke(strs));
        row("explicit-array", () -> (Object) MethodHandles.explicitCastArguments(va, MethodType.methodType(Object.class, Object.class)).invoke(objsArr));
        row("va-again-object-site", () -> (Object) va.invoke(strs));
    }
}
