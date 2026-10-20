// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L4: a null `arrayType` given to the
// two-argument `MethodHandle.asCollector(Class, int)` / `asSpreader(Class,
// int)` and to `asVarargsCollector(Class)`.
//
// JDK 25: `asCollector(arrayType, n)` -> `asCollector(pos, arrayType, n)` ->
// `asCollectorChecks` -> `spreadArrayChecks`, whose first statement is
// `arrayType.getComponentType()` (NPE); `asSpreader(arrayType, n)` ->
// `asSpreaderChecks` -> `spreadArrayChecks` (NPE); `asVarargsCollector`
// starts with `Objects.requireNonNull(arrayType)` (NPE, null message).
// CratonVM `--compatible` served the two-argument natives and
// `asVarargsCollector` without that check: a collector / spreader over
// `Object[]` was built and `asVarargsCollector` marked the handle.
// The three-argument forms already threw (control rows).
//
// Only exception class names are printed (the helpful-NPE text depends on
// the JDK's debug info).
//
// Run: javac L4W100NullArrayTypeAdapters.java && java L4W100NullArrayTypeAdapters
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// asCollector-null java.lang.NullPointerException
// asSpreader-null java.lang.NullPointerException
// asVarargsCollector-null java.lang.NullPointerException
// asCollector3-null java.lang.NullPointerException
// asSpreader3-null java.lang.NullPointerException
// asCollector-bound-null java.lang.NullPointerException
// asSpreader-bound-null java.lang.NullPointerException
// asCollector-ok 3
// asSpreader-ok ab
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W100NullArrayTypeAdapters {
    static int count(Object[] xs) {
        return xs.length;
    }

    static String cat(String a, String b) {
        return a + b;
    }

    static String cat3(String p, String a, String b) {
        return p + a + b;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            out = v instanceof MethodHandle ? "built " + ((MethodHandle) v).type() : String.valueOf(v);
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + " " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W100NullArrayTypeAdapters.class;
        MethodHandle count = l.findStatic(me, "count", MethodType.methodType(int.class, Object[].class));
        MethodHandle cat = l.findStatic(me, "cat",
                MethodType.methodType(String.class, String.class, String.class));
        MethodHandle cat3 = l.findStatic(me, "cat3",
                MethodType.methodType(String.class, String.class, String.class, String.class));
        MethodHandle bound = cat3.bindTo("p");

        row("asCollector-null", () -> count.asCollector(null, 2));
        row("asSpreader-null", () -> cat.asSpreader(null, 2));
        row("asVarargsCollector-null", () -> count.asVarargsCollector(null));
        row("asCollector3-null", () -> count.asCollector(0, null, 2));
        row("asSpreader3-null", () -> cat.asSpreader(0, null, 2));
        row("asCollector-bound-null", () -> bound.asCollector(null, 1));
        row("asSpreader-bound-null", () -> bound.asSpreader(null, 1));
        row("asCollector-ok", () -> (int) count.asCollector(Object[].class, 3).invoke("a", "b", "c"));
        row("asSpreader-ok", () -> (String) cat.asSpreader(String[].class, 2).invoke(new String[] {"a", "b"}));
    }
}
