// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 66, lane L3 --
// docs/internal/fixed-bugs/interpreter-i65-L3-proposal-the-carrier-handle-keeps-its-door-key-KEPT-OFF-20261029.md
// (`CRATONVM_THROWABLE_SERVED_RAISE_CARRIER_DOOR_KEY`, default off; takes
// effect only together with `CRATONVM_THROWABLE_SERVED_RAISE_SETTLED_DOOR_KEY`).
//
// The `--compatible` carrier handle (what `Lookup.findStatic` returns there;
// settled owner in the heap slot `MH_STATIC_SETTLED`) keeps its door's
// served-raise selection in a per-thread side line keyed by the carrier's
// address, its member objects and the collection epoch. The rows check the
// record a kept key writes is the record a fresh note writes, on the cases
// the side line adds: a warm carrier raising; the same carrier raising after
// a collection (a new epoch: the line misses and refills); the `invoke`
// (asType) door of a warm carrier; twenty carriers on the same method,
// warmed round-robin so they evict each other's lines, each raising.
//
// Every row must match HotSpot in both modes, with the two switches unset
// and both `=1`, JIT and `--nojit`. Rows print the throwable's frames up to
// the probe's own lambda (or `main`), no lines.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// carrier-warm: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W66CarrierDoorKey.lambda$main$0
// carrier-after-gc: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W66CarrierDoorKey.lambda$main$1
// carrier-invoke-warm: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W66CarrierDoorKey.lambda$main$2
// carrier-many-last: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W66CarrierDoorKey.lambda$main$3
// carrier-many-each: 20 raised, 1 distinct: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W66CarrierDoorKey.main
// warm-calls: 5001
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W66CarrierDoorKey`,
// CratonVM with nothing set and with
// `CRATONVM_THROWABLE_SERVED_RAISE_SETTLED_DOOR_KEY=1 CRATONVM_THROWABLE_SERVED_RAISE_CARRIER_DOOR_KEY=1`.
// Positive control: `CRATONVM_FRAME_TRACE=1` with both on prints
// `[SERVED_RAISE_DOOR_KEY] enter ... settled-slot=hit` for the warm carrier
// rows under `--compatible` (wave 65 printed it only under `--jdk-only`);
// with only `SETTLED_DOOR_KEY=1` the `--compatible` rows print no
// `settled-slot=hit`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.LinkedHashSet;
import java.util.Set;

public class L3W66CarrierDoorKey {
    static final int WARM = 2000;
    static final int MANY = 20;
    static final int MANY_WARM = 50;

    interface Row {
        Object run() throws Throwable;
    }

    static int warmCalls;

    static String frames(Throwable t) {
        StringBuilder b = new StringBuilder(t.getClass().getName() + " ");
        String sep = "";
        for (StackTraceElement e : t.getStackTrace()) {
            b.append(sep).append(e.getClassName()).append('.').append(e.getMethodName())
                    .append(e.isNativeMethod() ? "(N)" : "");
            sep = " | ";
            if (e.getMethodName().startsWith("lambda$") || e.getMethodName().equals("main")) {
                break;
            }
        }
        return b.toString();
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            System.out.println(name + ": " + frames(t));
        }
    }

    public static void main(String[] args) throws Throwable {
        ClassLoader app = L3W66CarrierDoorKey.class.getClassLoader();
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType forNameType =
                MethodType.methodType(Class.class, String.class, boolean.class, ClassLoader.class);
        MethodHandle forName = lookup.findStatic(Class.class, "forName", forNameType);
        MethodHandle forNameInvoke = lookup.findStatic(Class.class, "forName", forNameType);
        MethodHandle[] many = new MethodHandle[MANY];
        for (int i = 0; i < MANY; i++) {
            many[i] = lookup.findStatic(Class.class, "forName", forNameType);
        }
        String present = "java.lang.String";
        row("carrier-warm", () -> {
            for (int i = 0; i < WARM; i++) {
                Class<?> c = (Class<?>) forName.invokeExact(present, false, app);
                warmCalls++;
            }
            return (Class<?>) forName.invokeExact("no.Such", false, app);
        });
        row("carrier-after-gc", () -> {
            System.gc();
            Class<?> c = (Class<?>) forName.invokeExact(present, false, app);
            warmCalls++;
            System.gc();
            return (Class<?>) forName.invokeExact("no.Such.Gc", false, app);
        });
        row("carrier-invoke-warm", () -> {
            for (int i = 0; i < WARM; i++) {
                Object c = forNameInvoke.invoke(present, false, app);
                warmCalls++;
            }
            return forNameInvoke.invoke("no.Such.Invoke", false, app);
        });
        row("carrier-many-last", () -> {
            for (int round = 0; round < MANY_WARM; round++) {
                for (int i = 0; i < MANY; i++) {
                    Class<?> c = (Class<?>) many[i].invokeExact(present, false, app);
                    warmCalls++;
                }
            }
            return (Class<?>) many[MANY - 1].invokeExact("no.Such.Many", false, app);
        });
        Set<String> distinct = new LinkedHashSet<>();
        int raised = 0;
        for (int i = 0; i < MANY; i++) {
            try {
                Class<?> c = (Class<?>) many[i].invokeExact("no.Such.Each" + i, false, app);
            } catch (ClassNotFoundException e) {
                raised++;
                distinct.add(frames(e));
            }
        }
        System.out.println("carrier-many-each: " + raised + " raised, " + distinct.size()
                + " distinct: " + String.join(" || ", distinct));
        System.out.println("warm-calls: " + warmCalls);
    }
}
