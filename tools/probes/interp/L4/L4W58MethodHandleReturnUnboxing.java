// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L4: `MethodHandle.invoke` of a handle
// whose return is a REFERENCE at a PRIMITIVE call site is `asType`'s run-time
// unboxing (`ValueConversions.unbox*` -> `primitiveConversion`): `null` is a
// `NullPointerException`, a wrapper that does not widen to the site (or a
// `Number` that is no wrapper) is `Class.cast`'s `ClassCastException`,
// anything else the `(Number)` checkcast's. The same holds inside an
// `asType(()int)` copy of a `()Object` handle (`invokeExact` / `invoke`), an
// adapter over one, and an invoker of one. A `void` handle at an `int` site
// is `0`. CratonVM (wave 57) answered `0` for every refusal row. Page
// `docs/internal/fixed-bugs/interpreter-L4-a-method-handle-invoke-unboxes-a-null-or-foreign-box-to-zero-FIXED-20261022.md`.
//
// Run: javac -d out L4W58MethodHandleReturnUnboxing.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W58MethodHandleReturnUnboxing
//
// Expected HotSpot 25 output: see the block at the end of this file.
import java.lang.invoke.*;
import java.math.BigInteger;

public class L4W58MethodHandleReturnUnboxing {
    static Object objLong() { return 6L; }
    static Object objInt() { return 4; }
    static Object objShort() { return (short) 3; }
    static Object objChar() { return 'a'; }
    static Object objBool() { return Boolean.TRUE; }
    static Object objBig() { return BigInteger.ONE; }
    static Object objString() { return "s"; }
    static Object objArray() { return new int[] {1}; }
    static Object nul() { return null; }
    static Integer nulInteger() { return null; }
    static Integer boxedInt() { return 9; }
    static Number nulNumber() { return null; }
    static Number numLong() { return 12L; }
    static void nothing() { }

    interface Body { Object run() throws Throwable; }

    static void row(String name, Body b) {
        String out;
        try { out = String.valueOf(b.run()); } catch (Throwable t) { out = t.getClass().getName() + ": " + t.getMessage(); }
        System.out.println(name + ": " + out);
    }

    static MethodHandle h(String name, Class<?> ret) throws Exception {
        return MethodHandles.lookup().findStatic(L4W58MethodHandleReturnUnboxing.class, name, MethodType.methodType(ret));
    }

    public static void main(String[] a) throws Throwable {
        MethodHandle objLong = h("objLong", Object.class);
        MethodHandle objInt = h("objInt", Object.class);
        MethodHandle objShort = h("objShort", Object.class);
        MethodHandle objChar = h("objChar", Object.class);
        MethodHandle objBool = h("objBool", Object.class);
        MethodHandle objBig = h("objBig", Object.class);
        MethodHandle objString = h("objString", Object.class);
        MethodHandle objArray = h("objArray", Object.class);
        MethodHandle nul = h("nul", Object.class);
        MethodHandle nulInteger = h("nulInteger", Integer.class);
        MethodHandle boxedInt = h("boxedInt", Integer.class);
        MethodHandle nulNumber = h("nulNumber", Number.class);
        MethodHandle numLong = h("numLong", Number.class);
        MethodHandle nothing = h("nothing", void.class);
        MethodType toInt = MethodType.methodType(int.class);

        row("object-long-as-int-first", () -> (int) objLong.invoke());
        row("null-object-as-int-first", () -> (int) nul.invoke());
        row("object-int-as-int", () -> (int) objInt.invoke());
        row("object-int-as-long", () -> (long) objInt.invoke());
        row("object-int-as-double", () -> (double) objInt.invoke());
        row("object-short-as-int", () -> (int) objShort.invoke());
        row("object-short-as-long", () -> (long) objShort.invoke());
        row("object-char-as-int", () -> (int) objChar.invoke());
        row("object-int-as-char", () -> (int) (char) objInt.invoke());
        row("object-int-as-short", () -> (short) objInt.invoke());
        row("object-bool-as-boolean", () -> (boolean) objBool.invoke());
        row("object-int-as-boolean", () -> (boolean) objInt.invoke());
        row("object-bool-as-int", () -> (int) objBool.invoke());
        row("object-long-as-long", () -> (long) objLong.invoke());
        row("object-long-as-float", () -> (float) objLong.invoke());
        row("object-big-as-long", () -> (long) objBig.invoke());
        row("object-string-as-int", () -> (int) objString.invoke());
        row("object-array-as-int", () -> (int) objArray.invoke());
        row("null-object-as-int", () -> (int) nul.invoke());
        row("null-object-as-long", () -> (long) nul.invoke());
        row("null-object-as-boolean", () -> (boolean) nul.invoke());
        row("null-object-as-double", () -> (double) nul.invoke());
        row("null-integer-as-int", () -> (int) nulInteger.invoke());
        row("null-integer-as-long", () -> (long) nulInteger.invoke());
        row("boxed-integer-as-long", () -> (long) boxedInt.invoke());
        row("null-number-as-int", () -> (int) nulNumber.invoke());
        row("number-long-as-int", () -> (int) numLong.invoke());
        row("number-long-as-long", () -> (long) numLong.invoke());
        row("void-as-int", () -> (int) nothing.invoke());
        row("void-as-long", () -> (long) nothing.invoke());
        // Inside an `asType` copy, called exactly and through `invoke`.
        row("astype-long-exact", () -> (int) objLong.asType(toInt).invokeExact());
        row("astype-long-invoke", () -> (int) objLong.asType(toInt).invoke());
        row("astype-long-invoke-long", () -> (long) objLong.asType(toInt).invoke());
        row("astype-null-exact", () -> (int) nul.asType(toInt).invokeExact());
        row("astype-int-exact", () -> (int) objInt.asType(toInt).invokeExact());
        row("astype-short-exact", () -> (int) objShort.asType(toInt).invokeExact());
        row("astype-long-iwa", () -> objLong.asType(toInt).invokeWithArguments());
        row("astype-null-iwa", () -> nul.asType(toInt).invokeWithArguments());
        row("astype-int-iwa", () -> objInt.asType(toInt).invokeWithArguments(java.util.List.of()));
        row("astype-long-iwa-list", () -> objLong.asType(toInt).invokeWithArguments(java.util.List.of()));
        // Adapters over a `()Object` handle.
        row("drop-long-as-int", () -> (int) MethodHandles.dropArguments(objLong, 0, int.class).invoke(1));
        row("drop-int-as-int", () -> (int) MethodHandles.dropArguments(objInt, 0, int.class).invoke(1));
        row("filter-long-as-int", () -> (int) MethodHandles.filterReturnValue(objLong, MethodHandles.identity(Object.class)).invoke());
        row("insert-long-as-int", () -> (int) MethodHandles.insertArguments(MethodHandles.dropArguments(objLong, 0, String.class), 0, "x").invoke());
        row("invoker-long-as-int", () -> (int) MethodHandles.invoker(toInt).invokeExact(objLong));
        row("invoker-int-as-int", () -> (int) MethodHandles.invoker(toInt).invokeExact(objInt));
        row("invoker-null-as-int", () -> (int) MethodHandles.invoker(toInt).invokeExact(nul));
        row("invoker-astype-long-as-int", () -> (int) MethodHandles.invoker(toInt).invokeExact(objLong.asType(toInt)));
        row("invoker-astype-null-as-int", () -> (int) MethodHandles.invoker(toInt).invokeExact(nul.asType(toInt)));
        row("exact-invoker-long-as-int", () -> (int) MethodHandles.exactInvoker(toInt).invokeExact(objLong.asType(toInt)));
        // `explicitCastArguments` unboxes as JLS 5.5 casts: `null` is zero.
        row("explicit-null-exact", () -> (int) MethodHandles.explicitCastArguments(nul, toInt).invokeExact());
        row("explicit-long-exact", () -> (int) MethodHandles.explicitCastArguments(objLong, toInt).invokeExact());
        row("explicit-null-invoke", () -> (int) MethodHandles.explicitCastArguments(nul, toInt).invoke());
        row("object-long-as-int-again", () -> (int) objLong.invoke());
        row("null-object-as-int-again", () -> (int) nul.invoke());
    }
}




// Expected HotSpot 25 output (Temurin 25.0.3, default and -Xint identical):
//   object-long-as-int-first: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   null-object-as-int-first: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   object-int-as-int: 4
//   object-int-as-long: 4
//   object-int-as-double: 4.0
//   object-short-as-int: 3
//   object-short-as-long: 3
//   object-char-as-int: 97
//   object-int-as-char: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Character
//   object-int-as-short: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Short
//   object-bool-as-boolean: true
//   object-int-as-boolean: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Boolean
//   object-bool-as-int: java.lang.ClassCastException: Cannot cast java.lang.Boolean to java.lang.Integer
//   object-long-as-long: 6
//   object-long-as-float: 6.0
//   object-big-as-long: java.lang.ClassCastException: Cannot cast java.math.BigInteger to java.lang.Long
//   object-string-as-int: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   object-array-as-int: java.lang.ClassCastException: class [I cannot be cast to class java.lang.Number ([I and java.lang.Number are in module java.base of loader 'bootstrap')
//   null-object-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   null-object-as-long: java.lang.NullPointerException: Cannot invoke "java.lang.Number.longValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   null-object-as-boolean: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   null-object-as-double: java.lang.NullPointerException: Cannot invoke "java.lang.Number.doubleValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   null-integer-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
//   null-integer-as-long: java.lang.NullPointerException: Cannot invoke "java.lang.Number.longValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   boxed-integer-as-long: 9
//   null-number-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   number-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   number-long-as-long: 12
//   void-as-int: 0
//   void-as-long: 0
//   astype-long-exact: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   astype-long-invoke: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   astype-long-invoke-long: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   astype-null-exact: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   astype-int-exact: 4
//   astype-short-exact: 3
//   astype-long-iwa: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   astype-null-iwa: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   astype-int-iwa: 4
//   astype-long-iwa-list: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   drop-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   drop-int-as-int: 4
//   filter-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   insert-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   invoker-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   invoker-int-as-int: 4
//   invoker-null-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   invoker-astype-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   invoker-astype-null-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   exact-invoker-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   explicit-null-exact: 0
//   explicit-long-exact: 6
//   explicit-null-invoke: 0
//   object-long-as-int-again: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   null-object-as-int-again: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
