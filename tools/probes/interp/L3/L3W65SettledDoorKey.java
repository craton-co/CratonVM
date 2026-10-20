// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 65, lane L3 --
// docs/internal/fixed-bugs/interpreter-i64-L3-proposal-the-settling-handle-door-carries-its-door-key-KEPT-OFF-20261028.md
// (`CRATONVM_THROWABLE_SERVED_RAISE_SETTLED_DOOR_KEY`, default off) and
// docs/internal/fixed-bugs/interpreter-i64-L3-proposal-flip-the-door-key-and-retire-the-row-key-switch-DONE-20261028.md
// (`CRATONVM_THROWABLE_SERVED_RAISE_ROW_KEY` retired: the reflective door
// notes under `CRATONVM_THROWABLE_SERVED_RAISE_DOOR_KEY` alone).
//
// A settling `REF_invokeStatic` handle keeps the method its door's
// served-raise note selects on the handle record, so a WARM handle (one whose
// slot was filled by earlier calls that raised nothing) writes the door
// record with no lookup. The rows check the record a kept key writes is the
// record a fresh note writes: a handle called 2000 times without a raise,
// then raising, lists the same JDK frames as the cold handle of
// `L3W63DoorInFlightKey`; a second handle on the same method keeps its own
// slot; an application handle and a JDK method with bytecode stay unkeyed;
// the reflective row answers with `DOOR_KEY` alone (no `ROW_KEY`).
//
// The slot is filled only on the Rust-side handle records
// (`RealDirectTarget`, `LfEntryRecord` in `lang_invoke.rs`: real
// `DirectMethodHandle`s and lambda-form entries, `--jdk-only` above all);
// the `--compatible` carrier handle keeps today's per-call note (its heap
// record has no slot yet; see the page's "What remains"). Every row must
// match HotSpot in both modes, with the switch unset and `=1`, JIT and
// `--nojit`. Rows print the throwable's frames up to the probe's own lambda,
// no lines.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// handle-forName3-warm: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W65SettledDoorKey.lambda$main$0
// handle-forName3-warm-again: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W65SettledDoorKey.lambda$main$1
// handle-forName3-second-handle: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W65SettledDoorKey.lambda$main$2
// handle-app-warm: java.lang.IllegalStateException L3W65SettledDoorKey.maybeBoom | L3W65SettledDoorKey.lambda$main$3
// handle-jdk-bytecode-warm: java.lang.NullPointerException java.util.Objects.requireNonNull | L3W65SettledDoorKey.lambda$main$4
// reflect-forName3: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W65SettledDoorKey.lambda$main$5
// warm-sum: 2000
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W65SettledDoorKey`,
// CratonVM with and without `CRATONVM_THROWABLE_SERVED_RAISE_SETTLED_DOOR_KEY=1`.
// Positive control: `CRATONVM_FRAME_TRACE=1` with the switch on prints
// `[SERVED_RAISE_DOOR_KEY] enter ... settled-slot=hit` for the warm rows on
// a route that keeps the slot (`--jdk-only`), and the `door target ...
// keyed=true` line for the `handle-forName3*` raises either way.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Objects;

public class L3W65SettledDoorKey {
    static final int WARM = 2000;

    interface Row {
        Object run() throws Throwable;
    }

    static int warmSum;

    static Object maybeBoom(int i) {
        if (i < 0) {
            throw new IllegalStateException("app");
        }
        warmSum++;
        return null;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            StringBuilder b = new StringBuilder(name + ": " + t.getClass().getName() + " ");
            String sep = "";
            for (StackTraceElement e : t.getStackTrace()) {
                b.append(sep).append(e.getClassName()).append('.').append(e.getMethodName())
                        .append(e.isNativeMethod() ? "(N)" : "");
                sep = " | ";
                if (e.getMethodName().startsWith("lambda$")) {
                    break;
                }
            }
            System.out.println(b);
        }
    }

    public static void main(String[] args) throws Throwable {
        ClassLoader app = L3W65SettledDoorKey.class.getClassLoader();
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType forNameType =
                MethodType.methodType(Class.class, String.class, boolean.class, ClassLoader.class);
        MethodHandle forName = lookup.findStatic(Class.class, "forName", forNameType);
        MethodHandle forNameTwo = lookup.findStatic(Class.class, "forName", forNameType);
        MethodHandle maybeBoom = lookup.findStatic(L3W65SettledDoorKey.class, "maybeBoom",
                MethodType.methodType(Object.class, int.class));
        MethodHandle requireNonNull = lookup.findStatic(Objects.class, "requireNonNull",
                MethodType.methodType(Object.class, Object.class));
        Method reflectForName = Class.class.getMethod("forName",
                String.class, boolean.class, ClassLoader.class);
        String present = "java.lang.String";
        row("handle-forName3-warm", () -> {
            for (int i = 0; i < WARM; i++) {
                Class<?> c = (Class<?>) forName.invokeExact(present, false, app);
            }
            return (Class<?>) forName.invokeExact("no.Such", false, app);
        });
        row("handle-forName3-warm-again", () -> (Class<?>) forName.invokeExact("no.Such", false, app));
        row("handle-forName3-second-handle", () -> {
            Class<?> c = (Class<?>) forNameTwo.invokeExact(present, false, app);
            return (Class<?>) forNameTwo.invokeExact("no.Such.Two", false, app);
        });
        row("handle-app-warm", () -> {
            for (int i = 0; i < WARM; i++) {
                Object o = (Object) maybeBoom.invokeExact(i);
            }
            return (Object) maybeBoom.invokeExact(-1);
        });
        row("handle-jdk-bytecode-warm", () -> {
            Object x = "x";
            for (int i = 0; i < WARM; i++) {
                x = (Object) requireNonNull.invokeExact(x);
            }
            return (Object) requireNonNull.invokeExact((Object) null);
        });
        row("reflect-forName3", () -> {
            try {
                return reflectForName.invoke(null, "no.Such", false, app);
            } catch (InvocationTargetException e) {
                throw e.getCause();
            }
        });
        System.out.println("warm-sum: " + warmSum);
    }
}
