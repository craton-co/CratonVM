// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L4: a lambda whose implementation returns
// a reference that is NOT a wrapper (`Object`, `Number`, `String`) into a
// PRIMITIVE functional-interface return. HotSpot's spun class casts the value
// to `Number` (`Character` for `char`, `Boolean` for `boolean`) and unboxes it
// through that class, so a `Long` narrows by `Number.intValue()`, a
// `BigDecimal` runs its own `intValue()`, and a `String` is a
// `ClassCastException` naming `java.lang.Number`. CratonVM read field 0 of
// whatever came back (`coerce_return`), which handed a `String`'s `value`
// array to the return slot and PANICKED the VM (`L4W49ReflectiveMetafactory`
// row `bad-return`), and a `Long` arrived as a `long` in an `int` slot.
// Each shape is linked through `LambdaMetafactory.metafactory` called
// directly (javac never emits these sites); the first row runs twice, first
// and last.
//
// Run: javac -d out L4W50LambdaReturnUnbox.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W50LambdaReturnUnbox
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   string-int: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   long-int: 1
//   double-int: 3
//   bigdecimal-int: 12
//   integer-long: 7
//   object-char: x
//   string-char: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Character (java.lang.String and java.lang.Character are in module java.base of loader 'bootstrap')
//   object-boolean: true
//   integer-boolean: java.lang.ClassCastException: class java.lang.Integer cannot be cast to class java.lang.Boolean (java.lang.Integer and java.lang.Boolean are in module java.base of loader 'bootstrap')
//   null-int: java.lang.NullPointerException
//   string-int-again: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.math.BigDecimal;
import java.util.function.BooleanSupplier;
import java.util.function.IntSupplier;
import java.util.function.LongSupplier;

public class L4W50LambdaReturnUnbox {
    interface CharSupplier {
        char get();
    }

    static Object string() {
        return "s";
    }

    static Object longValue() {
        return 4294967297L;
    }

    static Number doubleValue() {
        return 3.9d;
    }

    static Number bigDecimal() {
        return new BigDecimal("12.75");
    }

    static Object integer() {
        return 7;
    }

    static Object character() {
        return 'x';
    }

    static Object bool() {
        return Boolean.TRUE;
    }

    static Object nothing() {
        return null;
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    static final MethodHandles.Lookup L = MethodHandles.lookup();

    static Object make(Class<?> iface, String sam, Class<?> samReturn, String impl, Class<?> implReturn)
            throws Throwable {
        MethodHandle h = L.findStatic(L4W50LambdaReturnUnbox.class, impl, MethodType.methodType(implReturn));
        CallSite cs = LambdaMetafactory.metafactory(L, sam, MethodType.methodType(iface),
            MethodType.methodType(samReturn), h, MethodType.methodType(samReturn));
        return cs.getTarget().invoke();
    }

    static Object stringInt() throws Throwable {
        return ((IntSupplier) make(IntSupplier.class, "getAsInt", int.class, "string", Object.class)).getAsInt();
    }

    public static void main(String[] args) {
        row("string-int", L4W50LambdaReturnUnbox::stringInt);
        row("long-int", () ->
            ((IntSupplier) make(IntSupplier.class, "getAsInt", int.class, "longValue", Object.class)).getAsInt());
        row("double-int", () ->
            ((IntSupplier) make(IntSupplier.class, "getAsInt", int.class, "doubleValue", Number.class)).getAsInt());
        row("bigdecimal-int", () ->
            ((IntSupplier) make(IntSupplier.class, "getAsInt", int.class, "bigDecimal", Number.class)).getAsInt());
        row("integer-long", () ->
            ((LongSupplier) make(LongSupplier.class, "getAsLong", long.class, "integer", Object.class)).getAsLong());
        row("object-char", () ->
            ((CharSupplier) make(CharSupplier.class, "get", char.class, "character", Object.class)).get());
        row("string-char", () ->
            ((CharSupplier) make(CharSupplier.class, "get", char.class, "string", Object.class)).get());
        row("object-boolean", () ->
            ((BooleanSupplier) make(BooleanSupplier.class, "getAsBoolean", boolean.class, "bool", Object.class))
                .getAsBoolean());
        row("integer-boolean", () ->
            ((BooleanSupplier) make(BooleanSupplier.class, "getAsBoolean", boolean.class, "integer", Object.class))
                .getAsBoolean());
        row("null-int", () ->
            ((IntSupplier) make(IntSupplier.class, "getAsInt", int.class, "nothing", Object.class)).getAsInt());
        row("string-int-again", L4W50LambdaReturnUnbox::stringInt);
    }
}
