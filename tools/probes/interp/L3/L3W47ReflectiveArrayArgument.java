// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L3 -- a reflective call (`Method.invoke`,
// `Constructor.newInstance`) handed an argument that is not assignable to an
// ARRAY-typed parameter. HotSpot's method handle casts every argument to its
// parameter type, so the call is refused with `IllegalArgumentException:
// argument type mismatch` before the target runs (the constructor keeps the
// `ClassCastException` as its cause, `Method.invoke` keeps none).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// m-integers-to-strings: java.lang.IllegalArgumentException: argument type mismatch | cause=none
// m-objects-to-strings: java.lang.IllegalArgumentException: argument type mismatch | cause=none
// m-string-to-strings: java.lang.IllegalArgumentException: argument type mismatch | cause=none
// m-longs-to-ints: java.lang.IllegalArgumentException: argument type mismatch | cause=none
// m-integer-to-ints: java.lang.IllegalArgumentException: argument type mismatch | cause=none
// m-ints-to-objects: java.lang.IllegalArgumentException: argument type mismatch | cause=none
// m-ints-to-matrix: java.lang.IllegalArgumentException: argument type mismatch | cause=none
// m-strings-ok: no exception: ran strings 1 [Ljava.lang.String;
// m-strings-to-objects-ok: no exception: ran objects 1 [Ljava.lang.String;
// m-null-ok: no exception: ran strings null
// c-integers-to-strings: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// c-ints-to-strings: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast [I to [Ljava.lang.String;
// c-strings-ok: no exception: made [Ljava.lang.String;
// HOTSPOT_EXPECTED_END
//
// CratonVM on the base a451e608e (read from the code): `coerce_arg_strict`
// judged a class or interface formal only; an ARRAY formal passed every
// reference through, so the `m-*` refusal rows printed `no exception: ran
// ...` (the target ran with a value of the wrong type: `ran strings 1
// [Ljava.lang.Integer;`, a `ClassCastException` or worse inside it for the
// others), and the `c-*` refusal rows `no exception: made ...`. Since wave 47
// (lane L3) a non-array argument is refused for an array formal and an array
// one by `checkcast`'s own predicate (`NativeClassAccess::array_value_casts_to_desc`),
// and the constructor's refusal carries the cast cause as for a class formal
// (`L3W47ConstructorArgCause`). Every row is expected to match, in both
// modes; the `*-ok` rows matched on the base and must keep matching.
//
// The wave-47 host run of the first cut refused EVERY array (the `*-ok`
// rows printed `argument type mismatch`, the cause named `java.lang.Integer`
// for an `Integer[]`): it recognised an array by its class NAME, but a
// reference array's header carries its COMPONENT's class id. Fixed on the
// same branch: array-ness is the heap's `object_is_array`, and the cause's
// name is rebuilt from the element kind or component
// (`lang_class::reflective_class_display_name`). Measured locally with
// Temurin 25.0.3+9; the host's HotSpot is 25.0.4+7.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W47ReflectiveArrayArgument`.
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;

public class L3W47ReflectiveArrayArgument {
    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            out = "no exception: " + call.run();
        } catch (Throwable t) {
            Throwable c = t.getCause();
            out = t + " | cause=" + (c == null ? "none" : c.toString());
        }
        System.out.println(name + ": " + out.replace("L3W47ReflectiveArrayArgument", "P"));
    }

    public static String strings(String[] a) {
        return "ran strings " + (a == null ? "null" : a.length + " " + a.getClass().getName());
    }

    public static String ints(int[] a) {
        return "ran ints " + a.length;
    }

    public static String objects(Object[] a) {
        return "ran objects " + a.length + " " + a.getClass().getName();
    }

    public static String matrix(int[][] a) {
        return "ran matrix " + a.length;
    }

    public static final class Made {
        final String what;

        public Made(String[] a) {
            what = "made " + a.getClass().getName();
        }

        @Override
        public String toString() {
            return what;
        }
    }

    public static void main(String[] args) throws Exception {
        Method strings = L3W47ReflectiveArrayArgument.class.getMethod("strings", String[].class);
        Method ints = L3W47ReflectiveArrayArgument.class.getMethod("ints", int[].class);
        Method objects = L3W47ReflectiveArrayArgument.class.getMethod("objects", Object[].class);
        Method matrix = L3W47ReflectiveArrayArgument.class.getMethod("matrix", int[][].class);
        Constructor<Made> made = Made.class.getConstructor(String[].class);

        row("m-integers-to-strings", () -> strings.invoke(null, (Object) new Integer[] {1}));
        row("m-objects-to-strings", () -> strings.invoke(null, (Object) new Object[] {"s"}));
        row("m-string-to-strings", () -> strings.invoke(null, "s"));
        row("m-longs-to-ints", () -> ints.invoke(null, (Object) new long[] {1}));
        row("m-integer-to-ints", () -> ints.invoke(null, 1));
        row("m-ints-to-objects", () -> objects.invoke(null, (Object) new int[] {1}));
        row("m-ints-to-matrix", () -> matrix.invoke(null, (Object) new int[] {1}));
        row("m-strings-ok", () -> strings.invoke(null, (Object) new String[] {"a"}));
        row("m-strings-to-objects-ok", () -> objects.invoke(null, (Object) new String[] {"a"}));
        row("m-null-ok", () -> strings.invoke(null, (Object) null));
        row("c-integers-to-strings", () -> made.newInstance((Object) new Integer[] {1}));
        row("c-ints-to-strings", () -> made.newInstance((Object) new int[] {1}));
        row("c-strings-ok", () -> made.newInstance((Object) new String[] {"a"}));
    }
}
