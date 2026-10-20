// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L4: `MethodHandles.tryFinally` and
// `iteratedLoop` with a null handle. The `--compatible` bridges
// (`register_method_handle_loop_bridge`) answered a NULL handle for
// `tryFinally(null, c)` and `iteratedLoop(it, init, null)`, and built an
// adapter for `tryFinally(t, null)`; JDK 25 raises a NullPointerException in
// each (`target.type()`, `cleanup.type()` in `tryFinallyChecks`,
// `Objects.requireNonNull(body)` in `iteratedLoopChecks`). `--jdk-only` runs
// the JDK's bytecode (the rows are retired there) and is the control.
//
// Run: javac -d out L4W106LoopFactoryNulls.java
//      java -cp out L4W106LoopFactoryNulls                     (HotSpot)
//      cratonvm --jdk-only -cp out L4W106LoopFactoryNulls
//      cratonvm --compatible -cp out L4W106LoopFactoryNulls
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, by reading the JDK source; the battery compares live)
// tryFinally-null-target: java.lang.NullPointerException: Cannot invoke "java.lang.invoke.MethodHandle.type()" because "target" is null
// tryFinally-null-cleanup: java.lang.NullPointerException: Cannot invoke "java.lang.invoke.MethodHandle.type()" because "cleanup" is null
// iteratedLoop-null-body: java.lang.NullPointerException: null
// iteratedLoop-null-all: java.lang.NullPointerException: null
// countedLoop-null-body: java.lang.NullPointerException: null
// tryFinally-ok: (int)int 7 cleanup-saw 7
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W106LoopFactoryNulls {
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

    static int seen;

    static int target(int x) {
        return x;
    }

    static int cleanup(Throwable t, int result, int x) {
        seen = result;
        return result;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle t = lk.findStatic(L4W106LoopFactoryNulls.class, "target",
                MethodType.methodType(int.class, int.class));
        MethodHandle c = lk.findStatic(L4W106LoopFactoryNulls.class, "cleanup",
                MethodType.methodType(int.class, Throwable.class, int.class, int.class));
        row("tryFinally-null-target", () -> MethodHandles.tryFinally(null, c));
        row("tryFinally-null-cleanup", () -> MethodHandles.tryFinally(t, null));
        row("iteratedLoop-null-body", () -> MethodHandles.iteratedLoop(null, null, null));
        row("iteratedLoop-null-all", () -> MethodHandles.iteratedLoop(
                null, MethodHandles.constant(int.class, 0), null));
        row("countedLoop-null-body", () -> MethodHandles.countedLoop(
                MethodHandles.constant(int.class, 3), null, null));
        row("tryFinally-ok", () -> {
            MethodHandle h = MethodHandles.tryFinally(t, c);
            int r = (int) h.invokeExact(7);
            return h.type() + " " + r + " cleanup-saw " + seen;
        });
    }
}
