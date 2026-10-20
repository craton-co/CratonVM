// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L3 (review) --
// docs/internal/fixed-bugs/interpreter-L3-a-reflective-door-s-class-init-failure-loses-its-jdk-frames-FIXED-20261022.md:
// a reflective door that initializes the member's class, whose static
// initializer throws. HotSpot raises the `ExceptionInInitializerError`
// (then `NoClassDefFoundError`) inside the accessor factory, under the native
// `Unsafe.ensureClassInitialized0`, and `Method.invoke` does NOT wrap it in
// an `InvocationTargetException` (the factory initializes the class outside
// the method handle on purpose).
//
// Each row prints the throwable, its cause, then the frames above the
// probe's first frame. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// field-get: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: a [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newFieldAccessor jdk.internal.reflect.ReflectionFactory.newFieldAccessor java.lang.reflect.Field.acquireFieldAccessor java.lang.reflect.Field.getFieldAccessor java.lang.reflect.Field.get]
// field-get-again: java.lang.NoClassDefFoundError cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: a [in thread "main"] [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newFieldAccessor jdk.internal.reflect.ReflectionFactory.newFieldAccessor java.lang.reflect.Field.acquireFieldAccessor java.lang.reflect.Field.getFieldAccessor java.lang.reflect.Field.get]
// field-setInt: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: b [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newFieldAccessor jdk.internal.reflect.ReflectionFactory.newFieldAccessor java.lang.reflect.Field.acquireFieldAccessor java.lang.reflect.Field.getFieldAccessor java.lang.reflect.Field.setInt]
// method-invoke: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: c [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newMethodAccessor jdk.internal.reflect.ReflectionFactory.newMethodAccessor java.lang.reflect.Method.acquireMethodAccessor java.lang.reflect.Method.invoke]
// ctor-newInstance: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: d [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newConstructorAccessor jdk.internal.reflect.ReflectionFactory.newConstructorAccessor java.lang.reflect.Constructor.acquireConstructorAccessor java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.Constructor.newInstance]
// lookup-ensureInitialized: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: e [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.MethodHandles$Lookup.ensureInitialized]
// forName: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: f [java.lang.Class.forName0 java.lang.Class.forName java.lang.Class.forName]
// field-get-again2: java.lang.NoClassDefFoundError cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: a [in thread "main"] [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newFieldAccessor jdk.internal.reflect.ReflectionFactory.newFieldAccessor java.lang.reflect.Field.acquireFieldAccessor java.lang.reflect.Field.getFieldAccessor java.lang.reflect.Field.get]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W57ReflectiveClassInitFailure`.
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Field;

public class L3W57ReflectiveClassInitFailure {
    static class A {
        static int x = 1;
        static {
            if (true) {
                throw new IllegalStateException("a");
            }
        }
    }

    static class B {
        static int y = 2;
        static {
            if (true) {
                throw new IllegalStateException("b");
            }
        }
    }

    static class C {
        static {
            if (true) {
                throw new IllegalStateException("c");
            }
        }

        static void m() {
        }
    }

    static class D {
        static {
            if (true) {
                throw new IllegalStateException("d");
            }
        }
    }

    static class E {
        static {
            if (true) {
                throw new IllegalStateException("e");
            }
        }
    }

    static class F {
        static int z = 3;
        static {
            if (true) {
                throw new IllegalStateException("f");
            }
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int k = 0;
            while (k < st.length && !st[k].getClassName().startsWith("L3W57")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            System.out.println(name + ": " + t.getClass().getName() + " cause=" + t.getCause() + " [" + jdk + "]");
        }
    }

    public static void main(String[] args) throws Exception {
        Field a = A.class.getDeclaredField("x");
        Field b = B.class.getDeclaredField("y");
        row("field-get", () -> a.get(null));
        row("field-get-again", () -> a.get(null));
        row("field-setInt", () -> {
            b.setInt(null, 3);
            return null;
        });
        row("method-invoke", () -> C.class.getDeclaredMethod("m").invoke(null));
        row("ctor-newInstance", () -> D.class.getDeclaredConstructor().newInstance());
        row("lookup-ensureInitialized", () -> MethodHandles.lookup().ensureInitialized(E.class));
        row("forName", () -> Class.forName("L3W57ReflectiveClassInitFailure$F"));
        Field a2 = A.class.getDeclaredField("x");
        row("field-get-again2", () -> a2.get(null));
    }
}
