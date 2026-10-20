// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 95, lane L4: `invokeWithArguments` on a
// variable-arity handle whose `AsVarargsCollector` override falls back to
// `super.invokeWithArguments(arguments)` (page
// `i94-orch-dev-jit-round-20-regresses-four-interpreter-probes`, rows of
// `L4W84InvokeWithArgumentsJumbo`).
//
// JDK 25 `MethodHandleImpl.AsVarargsCollector.invokeWithArguments(Object...)`
// calls `super.invokeWithArguments(arguments)` -- `MethodHandle`'s own body,
// `asType(genericMethodType(n))` plus a spread invoker -- when the list is
// short (`argc <= 127`), when it is shorter than the fixed parameters, and on
// the jumbo path when `System.arraycopy` into the trailing array raises
// `ArrayStoreException`. `genericMethodType(n)` refuses n > 255 with
// `IllegalArgumentException("bad parameter count " + n)`.
//
// CratonVM registers `MethodHandle.invokeWithArguments(Object...)` as a native,
// which the super call reaches; since dev's JIT round 20 a throwing `asType`
// declines to the native's fallback, which re-dispatched VIRTUALLY into the
// override again: `StackOverflowError` (both modes). The fallback now runs
// `MethodHandle`'s own body with invokespecial semantics for a collector.
//
// Run: javac -d . L4W95IwaVarargsSuper.java
//      java -cp . L4W95IwaVarargsSuper
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W95IwaVarargsSuper
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from the sources above)
// strs-300-ints: java.lang.IllegalArgumentException: bad parameter count 300
// strs-list-300-ints: java.lang.IllegalArgumentException: bad parameter count 300
// strs-256-ints: java.lang.IllegalArgumentException: bad parameter count 256
// strs-200-ints: java.lang.ClassCastException
// strs-300-strings: 300
// strs-3-strings: 3
// strs-0: 0
// lead-only: p:0
// lead-short: p:2
// sum-ints-300: 44850
// again-strs-300-ints: java.lang.IllegalArgumentException: bad parameter count 300
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;

public class L4W95IwaVarargsSuper {
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

    public static int strs(String... xs) {
        return xs.length;
    }

    public static String lead(String p, Object... xs) {
        return p + ":" + xs.length;
    }

    public static int sum(int... xs) {
        int s = 0;
        for (int x : xs) s += x;
        return s;
    }

    static Object[] ints(int n, int first) {
        Object[] a = new Object[n];
        for (int i = 0; i < n; i++) a[i] = first + i;
        return a;
    }

    static Object[] strings(int n) {
        Object[] a = new Object[n];
        for (int i = 0; i < n; i++) a[i] = "s" + i;
        return a;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle strs = l.findStatic(L4W95IwaVarargsSuper.class, "strs",
                MethodType.methodType(int.class, String[].class));
        MethodHandle lead = l.findStatic(L4W95IwaVarargsSuper.class, "lead",
                MethodType.methodType(String.class, String.class, Object[].class));
        MethodHandle sum = l.findStatic(L4W95IwaVarargsSuper.class, "sum",
                MethodType.methodType(int.class, int[].class));

        // Jumbo path, `System.arraycopy` of Integers into a String[] raises
        // ArrayStoreException -> super -> genericMethodType(n) refuses n > 255.
        row("strs-300-ints", true, () -> strs.invokeWithArguments(ints(300, 1)));
        row("strs-list-300-ints", true, () -> strs.invokeWithArguments(Arrays.asList(ints(300, 1))));
        row("strs-256-ints", true, () -> strs.invokeWithArguments(ints(256, 0)));
        // 200 fits a generic type: super's asType casts an Integer to String.
        row("strs-200-ints", false, () -> strs.invokeWithArguments(ints(200, 0)));
        // Jumbo path that succeeds (no super call).
        row("strs-300-strings", true, () -> strs.invokeWithArguments(strings(300)));
        // Short lists: the override calls super at once.
        row("strs-3-strings", true, () -> strs.invokeWithArguments(strings(3)));
        row("strs-0", true, () -> strs.invokeWithArguments());
        row("lead-only", true, () -> lead.invokeWithArguments("p"));
        row("lead-short", true, () -> lead.invokeWithArguments("p", 1, 2));
        // Primitive trailing array: the element-setter jumbo path.
        row("sum-ints-300", true, () -> sum.invokeWithArguments(ints(300, 0)));
        // The failing row once more: nothing left behind by the first.
        row("again-strs-300-ints", true, () -> strs.invokeWithArguments(ints(300, 1)));
    }
}
