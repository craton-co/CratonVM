// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L3 --
// docs/internal/fixed-bugs/interpreter-i62-L3-compatible-a-handle-to-a-served-static-method-whose-callee-built-the-throwable-lists-no-jdk-frames-FIXED-20261111.md
// (OPEN, `--compatible` only): the method-handle twin of
// `L3W62ReflectiveCalleeBuiltRaise`. A direct handle to the served
// `Class.forName(String, boolean, ClassLoader)` whose throwable the served
// loader (or a failed `<clinit>`) builds inside the native: HotSpot lists
// `Class.forName0` and `Class.forName` above the caller; `--compatible` lists
// neither. Rows print the throwable's frames up to the probe's own lambda, no
// lines. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// handle-forName3: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W62HandleCalleeBuiltRaise.lambda$main$0
// handle-forName3-init: java.lang.ExceptionInInitializerError java.lang.Class.forName0(N) | java.lang.Class.forName | L3W62HandleCalleeBuiltRaise.lambda$main$1
// handle-forName3-again: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | L3W62HandleCalleeBuiltRaise.lambda$main$2
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W62HandleCalleeBuiltRaise`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L3W62HandleCalleeBuiltRaise {
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
        ClassLoader app = L3W62HandleCalleeBuiltRaise.class.getClassLoader();
        MethodHandle h = MethodHandles.lookup().findStatic(Class.class, "forName",
                MethodType.methodType(Class.class, String.class, boolean.class, ClassLoader.class));
        String bad = L3W62HandleCalleeBuiltRaise.class.getName() + "$Bad";
        row("handle-forName3", () -> (Class<?>) h.invokeExact("no.Such", false, app));
        row("handle-forName3-init", () -> (Class<?>) h.invokeExact(bad, true, app));
        row("handle-forName3-again", () -> (Class<?>) h.invokeExact("no.Such", false, app));
    }
}
