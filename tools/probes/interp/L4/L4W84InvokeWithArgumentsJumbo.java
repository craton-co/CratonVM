// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L4: `MethodHandle.invokeWithArguments`
// with more arguments than a method type can carry (JDK 25 sources:
// `MethodHandle.invokeWithArguments(Object...)` is
// `asType(genericMethodType(n))` + a spread invoker, and
// `genericMethodType` -> `checkSlotCount` refuses n > 255 with
// `IllegalArgumentException("bad parameter count " + n)`;
// `MethodHandleImpl.AsVarargsCollector.invokeWithArguments` takes any list
// longer than 127 (`MAX_SAFE`) on its "jumbo" path: it collects the trailing
// arguments into the array itself -- `System.arraycopy` for a reference
// component, falling back to `super` on `ArrayStoreException`, and an
// `arrayElementSetter` per element for a primitive one, falling back on
// `WrongMethodTypeException` / `ClassCastException` only -- then calls the
// fixed-arity handle with that one array; `invokeWithArguments(List)` is
// `invokeWithArguments(arguments.toArray())`, a virtual call that reaches
// that override).
//
// Wave 84 fix: the `List` overload's native ran `MethodHandle`'s own body for a
// variable-arity handle, never the override, so every `list-*` row with more
// than 255 elements was `IllegalArgumentException: bad parameter count n`.
//
// Run: javac -d . L4W84InvokeWithArgumentsJumbo.java
//      cratonvm --jdk-only [--nojit] -cp . L4W84InvokeWithArgumentsJumbo
//      cratonvm --compatible [--nojit] -cp . L4W84InvokeWithArgumentsJumbo
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags; derived from the sources above)
// varargs-127: 127
// varargs-128: 128
// varargs-254: 254
// varargs-255: 255
// array-varargs-300: 300
// list-varargs-300: 300
// int-varargs-300: 45150
// list-int-varargs-300: 45150
// lead-varargs-200: p:199
// list-lead-varargs-300: p:299
// string-varargs-300-ints: java.lang.IllegalArgumentException: bad parameter count 300
// list-string-varargs-300-ints: java.lang.IllegalArgumentException: bad parameter count 300
// string-varargs-200-ints: java.lang.ClassCastException
// int-varargs-300-null: java.lang.NullPointerException
// fixed-list-300: java.lang.IllegalArgumentException: bad parameter count 300
// fixed-array-300: java.lang.IllegalArgumentException: bad parameter count 300
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.List;

public class L4W84InvokeWithArgumentsJumbo {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, boolean withMessage, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = withMessage ? t.getClass().getName() + ": " + t.getMessage() : t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    public static int sum(int... xs) {
        int s = 0;
        for (int x : xs) s += x;
        return s;
    }

    public static String lead(String p, Object... xs) {
        return p + ":" + xs.length;
    }

    public static int strs(String... xs) {
        return xs.length;
    }

    static Object[] ints(int n, int first) {
        Object[] a = new Object[n];
        for (int i = 0; i < n; i++) a[i] = first + i;
        return a;
    }

    static Object[] withLead(String p, int rest) {
        Object[] a = new Object[rest + 1];
        a[0] = p;
        for (int i = 0; i < rest; i++) a[i + 1] = i;
        return a;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle asList = l.findStatic(Arrays.class, "asList",
                MethodType.methodType(List.class, Object[].class));
        MethodHandle sum = l.findStatic(L4W84InvokeWithArgumentsJumbo.class, "sum",
                MethodType.methodType(int.class, int[].class));
        MethodHandle lead = l.findStatic(L4W84InvokeWithArgumentsJumbo.class, "lead",
                MethodType.methodType(String.class, String.class, Object[].class));
        MethodHandle strs = l.findStatic(L4W84InvokeWithArgumentsJumbo.class, "strs",
                MethodType.methodType(int.class, String[].class));
        MethodHandle fixed = MethodHandles.identity(Object[].class);

        for (int n : new int[] {127, 128, 254, 255}) {
            Object[] many = ints(n, 0);
            row("varargs-" + n, true, () -> ((List<?>) asList.invokeWithArguments(many)).size());
        }
        Object[] three = ints(300, 1);
        row("array-varargs-300", true, () -> ((List<?>) asList.invokeWithArguments(three)).size());
        row("list-varargs-300", true,
                () -> ((List<?>) asList.invokeWithArguments(Arrays.asList(three))).size());
        row("int-varargs-300", true, () -> sum.invokeWithArguments(three));
        row("list-int-varargs-300", true, () -> sum.invokeWithArguments(Arrays.asList(three)));
        row("lead-varargs-200", true, () -> lead.invokeWithArguments(withLead("p", 199)));
        row("list-lead-varargs-300", true,
                () -> lead.invokeWithArguments(Arrays.asList(withLead("p", 299))));
        row("string-varargs-300-ints", true, () -> strs.invokeWithArguments(three));
        row("list-string-varargs-300-ints", true,
                () -> strs.invokeWithArguments(Arrays.asList(three)));
        row("string-varargs-200-ints", false, () -> strs.invokeWithArguments(ints(200, 0)));
        Object[] withNull = ints(300, 1);
        withNull[150] = null;
        row("int-varargs-300-null", false, () -> sum.invokeWithArguments(withNull));
        row("fixed-list-300", true, () -> fixed.invokeWithArguments(Arrays.asList(three)));
        row("fixed-array-300", true, () -> fixed.invokeWithArguments(three));
    }
}
