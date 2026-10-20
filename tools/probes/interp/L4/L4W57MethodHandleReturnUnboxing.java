// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L4 (review): `MethodHandle.invoke` of a
// handle whose return is a REFERENCE at a PRIMITIVE call site is `asType`'s
// run-time unboxing (`ValueConversions.unbox*`): `null` is a
// `NullPointerException` and a wrapper that does not widen to the site is a
// `ClassCastException`. CratonVM (wave 57, local debug build, both modes)
// answers `0` for both (`null-object-as-int`, `object-long-as-int`); the
// static refusals already match. Page
// `docs/internal/fixed-bugs/interpreter-L4-a-method-handle-invoke-unboxes-a-null-or-foreign-box-to-zero-FIXED-20261022.md`.
//
// Run: javac -d out L4W57MethodHandleReturnUnboxing.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W57MethodHandleReturnUnboxing
//
// Expected HotSpot 25 output (Temurin 25.0.3, default and -Xint identical):
//   object-long-as-int-first: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   array-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()int[] to ()int
//   long-wrapper-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()Long to ()int
//   long-wrapper-as-long: 5
//   string-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()String to ()int
//   null-object-as-int: NPE
//   object-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   object-short-as-int: 3
//   boolean-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()boolean to ()int
//   long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()long to ()int
//   long-as-double: 7.0
//   array-as-int-again: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()int[] to ()int
import java.lang.invoke.*;

public class L4W57MethodHandleReturnUnboxing {
    static int[] arr() { return new int[] {1}; }
    static Long lng() { return 5L; }
    static String str() { return "s"; }
    static Object nul() { return null; }
    static Object objLong() { return 6L; }
    static Object objShort() { return (short) 3; }
    static boolean bool() { return true; }
    static long prim() { return 7L; }

    interface Body { Object run() throws Throwable; }

    static void row(String name, Body b) {
        String out;
        try { out = String.valueOf(b.run()); } catch (Throwable t) { out = t.getClass().getName() + ": " + t.getMessage(); }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle arr = lk.findStatic(L4W57MethodHandleReturnUnboxing.class, "arr", MethodType.methodType(int[].class));
        MethodHandle lng = lk.findStatic(L4W57MethodHandleReturnUnboxing.class, "lng", MethodType.methodType(Long.class));
        MethodHandle str = lk.findStatic(L4W57MethodHandleReturnUnboxing.class, "str", MethodType.methodType(String.class));
        MethodHandle nul = lk.findStatic(L4W57MethodHandleReturnUnboxing.class, "nul", MethodType.methodType(Object.class));
        MethodHandle objLong = lk.findStatic(L4W57MethodHandleReturnUnboxing.class, "objLong", MethodType.methodType(Object.class));
        MethodHandle objShort = lk.findStatic(L4W57MethodHandleReturnUnboxing.class, "objShort", MethodType.methodType(Object.class));
        MethodHandle bool = lk.findStatic(L4W57MethodHandleReturnUnboxing.class, "bool", MethodType.methodType(boolean.class));
        MethodHandle prim = lk.findStatic(L4W57MethodHandleReturnUnboxing.class, "prim", MethodType.methodType(long.class));
        row("object-long-as-int-first", () -> (int) objLong.invoke());
        row("array-as-int", () -> (int) arr.invoke());
        row("long-wrapper-as-int", () -> (int) lng.invoke());
        row("long-wrapper-as-long", () -> (long) lng.invoke());
        row("string-as-int", () -> (int) str.invoke());
        row("null-object-as-int", () -> { try { return (int) nul.invoke(); } catch (NullPointerException e) { return "NPE"; } });
        row("object-long-as-int", () -> (int) objLong.invoke());
        row("object-short-as-int", () -> (int) objShort.invoke());
        row("boolean-as-int", () -> (int) bool.invoke());
        row("long-as-int", () -> (int) prim.invoke());
        row("long-as-double", () -> (double) prim.invoke());
        row("array-as-int-again", () -> (int) arr.invoke());
    }
}
