// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 63, lane L3 --
// docs/internal/fixed-bugs/interpreter-i62-L3-proposal-one-door-in-flight-key-for-the-served-raise-families-DONE-20261014.md
// (`CRATONVM_THROWABLE_SERVED_RAISE_DOOR_KEY`, default off). A static JDK
// door notes the method it is entering for the length of its call, so a
// throwable a callee builds inside the served native lists the method's JDK
// frames. Written for the rows of the twin page
// `i62-L3-compatible-a-handle-to-a-served-static-method-whose-callee-built-the-throwable-lists-no-jdk-frames-20261026.md`.
// Wave-63 host run (JDK 25.0.4 oracle): the `handle-forName3*` rows already
// list `Class.forName0` / `Class.forName` with the switch unset, and are the
// same with `=1`; they still differ from HotSpot by a second `Class.forName`
// frame (`interpreter-L3-a-handle-to-a-caller-sensitive-method-runs-the-public-method-not-its-adapter-RETIRED-20261007.md`)
// and, on the class-not-found rows, an `AppClassLoader.loadClass` frame
// (`interpreter-L3-an-application-loader-miss-lists-an-appclassloader-loadclass-frame-RETIRED-20261007.md`).
// The controls (`handle-app`, `handle-jdk-bytecode`,
// `reflect-forName3`) must match with the switch either way: an application
// door notes nothing, a JDK method that pushes its own frame is not keyed,
// and the reflective row carrier (wave 62) still answers. `--jdk-only`
// matches with the switch either way. Rows print the throwable's frames up
// to the probe's own lambda, no lines.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// handle-forName3: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W63DoorInFlightKey.lambda$main$0
// handle-forName3-init: java.lang.ExceptionInInitializerError java.lang.Class.forName0(N) | java.lang.Class.forName | L3W63DoorInFlightKey.lambda$main$1
// handle-forName3-init-again: java.lang.NoClassDefFoundError java.lang.Class.forName0(N) | java.lang.Class.forName | L3W63DoorInFlightKey.lambda$main$2
// handle-app: java.lang.IllegalStateException L3W63DoorInFlightKey.boom | L3W63DoorInFlightKey.lambda$main$3
// handle-jdk-bytecode: java.lang.NullPointerException java.util.Objects.requireNonNull | L3W63DoorInFlightKey.lambda$main$4
// reflect-forName3: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W63DoorInFlightKey.lambda$main$5
// handle-forName3-repeat: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W63DoorInFlightKey.lambda$main$6
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W63DoorInFlightKey`,
// CratonVM with and without `CRATONVM_THROWABLE_SERVED_RAISE_DOOR_KEY=1`.
// Positive control: `CRATONVM_FRAME_TRACE=1` prints
// `[SERVED_RAISE_DOOR_KEY] door target ... keyed=true` for the
// `handle-forName3*` rows.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Objects;

public class L3W63DoorInFlightKey {
    interface Row {
        Object run() throws Throwable;
    }

    static class Bad {
        static {
            if (true) {
                throw new IllegalStateException("bad");
            }
        }
    }

    static Object boom(String s) {
        throw new IllegalStateException(s);
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
        ClassLoader app = L3W63DoorInFlightKey.class.getClassLoader();
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle forName = lookup.findStatic(Class.class, "forName",
                MethodType.methodType(Class.class, String.class, boolean.class, ClassLoader.class));
        MethodHandle boom = lookup.findStatic(L3W63DoorInFlightKey.class, "boom",
                MethodType.methodType(Object.class, String.class));
        MethodHandle requireNonNull = lookup.findStatic(Objects.class, "requireNonNull",
                MethodType.methodType(Object.class, Object.class));
        Method reflectForName = Class.class.getMethod("forName",
                String.class, boolean.class, ClassLoader.class);
        String bad = L3W63DoorInFlightKey.class.getName() + "$Bad";
        row("handle-forName3", () -> (Class<?>) forName.invokeExact("no.Such", false, app));
        row("handle-forName3-init", () -> (Class<?>) forName.invokeExact(bad, true, app));
        row("handle-forName3-init-again", () -> (Class<?>) forName.invokeExact(bad, true, app));
        row("handle-app", () -> (Object) boom.invokeExact("app"));
        row("handle-jdk-bytecode", () -> (Object) requireNonNull.invokeExact((Object) null));
        row("reflect-forName3", () -> {
            try {
                return reflectForName.invoke(null, "no.Such", false, app);
            } catch (InvocationTargetException e) {
                throw e.getCause();
            }
        });
        row("handle-forName3-repeat", () -> (Class<?>) forName.invokeExact("no.Such", false, app));
    }
}
