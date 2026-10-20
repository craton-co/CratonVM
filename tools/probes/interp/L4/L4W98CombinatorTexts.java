// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L4 -- page
// `docs/internal/fixed-bugs/interpreter-i98-L4-catch-exception-refusal-texts-FIXED-20261015.md`.
//
// `MethodHandles.catchException(target, exType, handler)` refuses, in this
// order (JDK 25 `MethodHandles.catchException`):
//   1. `!Throwable.class.isAssignableFrom(exType)` -> `ClassCastException(exType.getName())`
//   2. no handler parameter, or `!htype.parameterType(0).isAssignableFrom(exType)`
//      -> `IllegalArgumentException("handler does not accept exception type " + exType)`
//   3. return types / leading parameters differ -> `misMatchedTypes(...)`.
// `--compatible`'s native combinator printed rule 2 as `handler should adapt to
// java/lang/ClassCastException: (L...;...)...` whenever the target was not a
// plain direct handle (a `bindTo` result), accepted an interface `exType`, and
// accepted a handler whose first parameter is primitive or that has none.
//
// No setup.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the JDK sources)
// catch-ok: caught:x
// catch-wrong-handler: java.lang.IllegalArgumentException: handler does not accept exception type class java.lang.ClassCastException
// catch-wrong-handler-bound-target: java.lang.IllegalArgumentException: handler does not accept exception type class java.lang.ClassCastException
// catch-interface-type: java.lang.ClassCastException: java.lang.Runnable
// catch-interface-type-bound-target: java.lang.ClassCastException: java.lang.Runnable
// catch-primitive-handler: java.lang.IllegalArgumentException: handler does not accept exception type class java.lang.RuntimeException
// catch-no-param-handler: java.lang.IllegalArgumentException: handler does not accept exception type class java.lang.RuntimeException
// catch-interface-handler: ser:x
// catch-return-mismatch: java.lang.IllegalArgumentException: target and handler return types must match: (String)String != (RuntimeException,String)Object
// HOTSPOT_EXPECTED_END
import java.io.Serializable;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W98CombinatorTexts {
    static String boom(String s) {
        throw new ClassCastException(s);
    }

    String boomInstance(String s) {
        throw new ClassCastException(s);
    }

    static String onCce(ClassCastException e, String s) {
        return "caught:" + e.getMessage();
    }

    static String onIse(IllegalStateException e, String s) {
        return "ise";
    }

    static String onInt(int x) {
        return "int";
    }

    static String onNothing() {
        return "nothing";
    }

    static String onSerializable(Serializable e, String s) {
        return "ser:" + ((Throwable) e).getMessage();
    }

    static Object onRuntimeObject(RuntimeException e, String s) {
        return "obj";
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W98CombinatorTexts.class;
        MethodType ss = MethodType.methodType(String.class, String.class);
        MethodHandle target = l.findStatic(me, "boom", ss);
        MethodHandle bound = l.findVirtual(me, "boomInstance", ss).bindTo(new L4W98CombinatorTexts());
        MethodHandle onCce = l.findStatic(me, "onCce",
                MethodType.methodType(String.class, ClassCastException.class, String.class));
        MethodHandle onIse = l.findStatic(me, "onIse",
                MethodType.methodType(String.class, IllegalStateException.class, String.class));
        MethodHandle onInt = l.findStatic(me, "onInt", MethodType.methodType(String.class, int.class));
        MethodHandle onNothing = l.findStatic(me, "onNothing", MethodType.methodType(String.class));
        MethodHandle onSer = l.findStatic(me, "onSerializable",
                MethodType.methodType(String.class, Serializable.class, String.class));
        MethodHandle onObj = l.findStatic(me, "onRuntimeObject",
                MethodType.methodType(Object.class, RuntimeException.class, String.class));
        Class runnable = Runnable.class;

        row("catch-ok", () -> (String) MethodHandles.catchException(target, ClassCastException.class, onCce)
                .invokeExact("x"));
        row("catch-wrong-handler", () -> MethodHandles.catchException(target, ClassCastException.class, onIse));
        row("catch-wrong-handler-bound-target", () ->
                MethodHandles.catchException(bound, ClassCastException.class, onIse));
        row("catch-interface-type", () -> MethodHandles.catchException(target, runnable, onCce));
        row("catch-interface-type-bound-target", () -> MethodHandles.catchException(bound, runnable, onCce));
        row("catch-primitive-handler", () -> MethodHandles.catchException(target, RuntimeException.class, onInt));
        row("catch-no-param-handler", () ->
                MethodHandles.catchException(target, RuntimeException.class, onNothing));
        row("catch-interface-handler", () ->
                (String) MethodHandles.catchException(target, RuntimeException.class, onSer).invokeExact("x"));
        row("catch-return-mismatch", () -> MethodHandles.catchException(target, RuntimeException.class, onObj));
    }
}
