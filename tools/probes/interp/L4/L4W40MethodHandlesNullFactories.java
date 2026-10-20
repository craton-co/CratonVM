// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L4 (review): the `MethodHandles`
// factories CratonVM serves natively raise HotSpot's NullPointerException for
// a `null` class or type. CratonVM answered a handle of a default type
// (`Object[]` accessors, `(Object)Object`, `()Object`, `()V`), so the row's
// `type()` printed instead.
//
// Run: javac -d out L4W40MethodHandlesNullFactories.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W40MethodHandlesNullFactories
//
// Expected HotSpot 25 output (default and -Xint; `--compatible` identical):
//   arrayLength-null: java.lang.NullPointerException: Cannot invoke "java.lang.Class.isArray()" because "arrayClass" is null
//   arrayGetter-null: java.lang.NullPointerException: Cannot invoke "java.lang.Class.isArray()" because "arrayClass" is null
//   arraySetter-null: java.lang.NullPointerException: Cannot invoke "java.lang.Class.isArray()" because "arrayClass" is null
//   constant-null: java.lang.NullPointerException: null
//   identity-null: java.lang.NullPointerException: Cannot invoke "java.lang.Class.isPrimitive()" because "type" is null
//   zero-null: java.lang.NullPointerException: null
//   empty-null: java.lang.NullPointerException: null
//   zero-void: ()void
//   identity-bind-null: null
//   empty-bind-invoke: (int)String null
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W40MethodHandlesNullFactories {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            out = v instanceof MethodHandle h ? "handle " + h.type() : String.valueOf(v);
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        row("arrayLength-null", () -> MethodHandles.arrayLength(null));
        row("arrayGetter-null", () -> MethodHandles.arrayElementGetter(null));
        row("arraySetter-null", () -> MethodHandles.arrayElementSetter(null));
        row("constant-null", () -> MethodHandles.constant(null, 1));
        row("identity-null", () -> MethodHandles.identity(null));
        row("zero-null", () -> MethodHandles.zero(null));
        row("empty-null", () -> MethodHandles.empty(null));
        row("zero-void", () -> MethodHandles.zero(void.class).type());
        row("identity-bind-null", () -> {
            MethodHandle h = MethodHandles.identity(String.class).bindTo(null);
            return (String) h.invokeExact();
        });
        row("empty-bind-invoke", () -> {
            MethodHandle h = MethodHandles.empty(MethodType.methodType(String.class, Object.class, int.class))
                    .bindTo("q");
            return h.type() + " " + (String) h.invokeExact(3);
        });
    }
}
