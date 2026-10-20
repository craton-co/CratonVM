// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-proposal-the-served-raise-mark-believes-only-its-own-construction-FIXED-20261026.md:
// a reflective door initializes a class whose `<clinit>` first triggers
// ANOTHER class's failing initialization (an `ExceptionInInitializerError`
// of the same class, caught inside), then fails itself. HotSpot lists the
// accessor factory's frames for the OUTER failure; the inner one, built while
// the door's mark stood, must not take the mark. Each row prints the inner
// error the `<clinit>` caught (class and its top frame), then the outer
// throwable, its cause and its JDK frames above the probe. The first row is
// repeated last (a class of its own each time).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// field-get inner: java.lang.ExceptionInInitializerError at L3W62MarkOwnConstruction$F1.<clinit>
// field-get: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: f1 [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newFieldAccessor jdk.internal.reflect.ReflectionFactory.newFieldAccessor java.lang.reflect.Field.acquireOverrideFieldAccessor java.lang.reflect.Field.getOverrideFieldAccessor java.lang.reflect.Field.get]
// method-invoke inner: java.lang.ExceptionInInitializerError at L3W62MarkOwnConstruction$M1.<clinit>
// method-invoke: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: m1 [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newMethodAccessor jdk.internal.reflect.ReflectionFactory.newMethodAccessor java.lang.reflect.Method.acquireMethodAccessor java.lang.reflect.Method.invoke]
// field-get-again inner: java.lang.ExceptionInInitializerError at L3W62MarkOwnConstruction$F2.<clinit>
// field-get-again: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: f2 [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newFieldAccessor jdk.internal.reflect.ReflectionFactory.newFieldAccessor java.lang.reflect.Field.acquireOverrideFieldAccessor java.lang.reflect.Field.getOverrideFieldAccessor java.lang.reflect.Field.get]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W62MarkOwnConstruction`.
import java.lang.reflect.Field;
import java.lang.reflect.Method;

public class L3W62MarkOwnConstruction {
    static String inner = "none";

    static void noteInner(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        inner = t.getClass().getName() + " at "
                + (st.length == 0 ? "?" : st[0].getClassName() + "." + st[0].getMethodName());
    }

    static class Y1 {
        static int y = 1;
        static {
            if (true) {
                throw new IllegalStateException("y1");
            }
        }
    }

    static class Y2 {
        static int y = 1;
        static {
            if (true) {
                throw new IllegalStateException("y2");
            }
        }
    }

    static class Y3 {
        static int y = 1;
        static {
            if (true) {
                throw new IllegalStateException("y3");
            }
        }
    }

    static class F1 {
        private static int f = 1;
        static {
            try {
                f = Y1.y;
            } catch (ExceptionInInitializerError e) {
                noteInner(e);
            }
            if (true) {
                throw new IllegalStateException("f1");
            }
        }
    }

    static class M1 {
        static {
            try {
                int y = Y2.y;
            } catch (ExceptionInInitializerError e) {
                noteInner(e);
            }
            if (true) {
                throw new IllegalStateException("m1");
            }
        }

        static void m() {
        }
    }

    static class F2 {
        private static int f = 1;
        static {
            try {
                f = Y3.y;
            } catch (ExceptionInInitializerError e) {
                noteInner(e);
            }
            if (true) {
                throw new IllegalStateException("f2");
            }
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        inner = "none";
        try {
            Object v = r.run();
            System.out.println(name + " inner: " + inner);
            System.out.println(name + ": " + v);
        } catch (Throwable t) {
            System.out.println(name + " inner: " + inner);
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int k = 0;
            while (k < st.length && !st[k].getClassName().startsWith("L3W62")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            System.out.println(name + ": " + t.getClass().getName() + " cause=" + t.getCause() + " [" + jdk + "]");
        }
    }

    public static void main(String[] args) throws Exception {
        Field f1 = F1.class.getDeclaredField("f");
        f1.setAccessible(true);
        row("field-get", () -> f1.get(null));
        Method m1 = M1.class.getDeclaredMethod("m");
        row("method-invoke", () -> m1.invoke(null));
        Field f2 = F2.class.getDeclaredField("f");
        f2.setAccessible(true);
        row("field-get-again", () -> f2.get(null));
    }
}
