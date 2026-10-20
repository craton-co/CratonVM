// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-a-reflective-door-s-class-init-failure-loses-its-jdk-frames-FIXED-20261022.md:
// a reflective door on a member whose class's static initializer throws.
// JDK 25 initializes the class in its accessor factory, OUTSIDE the method
// handle, under `Unsafe.ensureClassInitialized0`: `Method.invoke` throws the
// `ExceptionInInitializerError` itself (CratonVM wrapped it in an
// `InvocationTargetException`), and every door lists the factory's frames.
// The rows add what the wave-57 probe did not: the override's accessor
// (`getOverrideFieldAccessor`), the NPE of a null receiver that the access
// check raises BEFORE the initialization, the `NoClassDefFoundError` through
// `Method.invoke` / `Constructor.newInstance`, and a class whose
// initialization succeeds (its `<clinit>` runs before the method), and the
// `Class.newInstance` / `Unsafe.allocateInstance` doors.
//
// Each failing row prints the throwable, its cause, then the frames above
// the probe's first frame. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// method-invoke: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: c [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newMethodAccessor jdk.internal.reflect.ReflectionFactory.newMethodAccessor java.lang.reflect.Method.acquireMethodAccessor java.lang.reflect.Method.invoke]
// method-invoke-again: java.lang.NoClassDefFoundError cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: c [in thread "main"] [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newMethodAccessor jdk.internal.reflect.ReflectionFactory.newMethodAccessor java.lang.reflect.Method.acquireMethodAccessor java.lang.reflect.Method.invoke]
// method-null-receiver: java.lang.NullPointerException cause=null [java.lang.reflect.Method.invoke]
// method-accessible: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: g [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newMethodAccessor jdk.internal.reflect.ReflectionFactory.newMethodAccessor java.lang.reflect.Method.acquireMethodAccessor java.lang.reflect.Method.invoke]
// field-get-accessible: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: h [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newFieldAccessor jdk.internal.reflect.ReflectionFactory.newFieldAccessor java.lang.reflect.Field.acquireOverrideFieldAccessor java.lang.reflect.Field.getOverrideFieldAccessor java.lang.reflect.Field.get]
// field-set-wrong-value: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: k [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newFieldAccessor jdk.internal.reflect.ReflectionFactory.newFieldAccessor java.lang.reflect.Field.acquireFieldAccessor java.lang.reflect.Field.getFieldAccessor java.lang.reflect.Field.set]
// ctor-again: java.lang.NoClassDefFoundError cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: d [in thread "main"] [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newConstructorAccessor jdk.internal.reflect.ReflectionFactory.newConstructorAccessor java.lang.reflect.Constructor.acquireConstructorAccessor java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.Constructor.newInstance]
// class-newInstance: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: n [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newConstructorAccessor jdk.internal.reflect.ReflectionFactory.newConstructorAccessor java.lang.reflect.Constructor.acquireConstructorAccessor java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.ReflectAccess.newInstance jdk.internal.reflect.ReflectionFactory.newInstance java.lang.Class.newInstance]
// class-newInstance-again: java.lang.NoClassDefFoundError cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: n [in thread "main"] [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newConstructorAccessor jdk.internal.reflect.ReflectionFactory.newConstructorAccessor java.lang.reflect.Constructor.acquireConstructorAccessor java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.ReflectAccess.newInstance jdk.internal.reflect.ReflectionFactory.newInstance java.lang.Class.newInstance]
// unsafe-allocate: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: u [jdk.internal.misc.Unsafe.allocateInstance sun.misc.Unsafe.allocateInstance]
// ok-clinit
// ok-invoke: 7
// method-target-throws: java.lang.reflect.InvocationTargetException cause=java.lang.IllegalStateException: t [jdk.internal.reflect.DirectMethodHandleAccessor.invoke java.lang.reflect.Method.invoke]
// method-invoke-repeat: java.lang.NoClassDefFoundError cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: c [in thread "main"] [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.ensureClassInitialized jdk.internal.reflect.MethodHandleAccessorFactory.newMethodAccessor jdk.internal.reflect.ReflectionFactory.newMethodAccessor java.lang.reflect.Method.acquireMethodAccessor java.lang.reflect.Method.invoke]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W58ReflectiveClassInit`.
import java.lang.reflect.Field;
import java.lang.reflect.Method;

public class L3W58ReflectiveClassInit {
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

    static class G {
        static {
            if (true) {
                throw new IllegalStateException("g");
            }
        }

        void inst() {
        }

        private static void p() {
        }
    }

    static class H {
        private static int h = 1;
        static {
            if (true) {
                throw new IllegalStateException("h");
            }
        }
    }

    static class K {
        static int k = 1;
        static {
            if (true) {
                throw new IllegalStateException("k");
            }
        }
    }

    static class N {
        static {
            if (true) {
                throw new IllegalStateException("n");
            }
        }

        public N() {
        }
    }

    static class U {
        static {
            if (true) {
                throw new IllegalStateException("u");
            }
        }
    }

    static class Ok {
        static {
            System.out.println("ok-clinit");
        }

        static int seven() {
            return 7;
        }
    }

    static class T {
        static void t() {
            throw new IllegalStateException("t");
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
            while (k < st.length && !st[k].getClassName().startsWith("L3W58")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            System.out.println(name + ": " + t.getClass().getName() + " cause=" + t.getCause() + " [" + jdk + "]");
        }
    }

    @SuppressWarnings("deprecation")
    public static void main(String[] args) throws Exception {
        row("method-invoke", () -> C.class.getDeclaredMethod("m").invoke(null));
        row("method-invoke-again", () -> C.class.getDeclaredMethod("m").invoke(null));
        // Not accessible: `checkAccess`'s `obj.getClass()` raises first, and
        // the class stays uninitialized for the next row.
        Method inst = G.class.getDeclaredMethod("inst");
        row("method-null-receiver", () -> inst.invoke(null));
        Method p = G.class.getDeclaredMethod("p");
        p.setAccessible(true);
        row("method-accessible", () -> p.invoke(null));
        Field h = H.class.getDeclaredField("h");
        h.setAccessible(true);
        row("field-get-accessible", () -> h.get(null));
        Field k = K.class.getDeclaredField("k");
        row("field-set-wrong-value", () -> {
            k.set(null, "not an int");
            return null;
        });
        try {
            D.class.getDeclaredConstructor().newInstance();
        } catch (Throwable ignored) {
            // the first failure; the row is the second
        }
        row("ctor-again", () -> D.class.getDeclaredConstructor().newInstance());
        row("class-newInstance", () -> N.class.newInstance());
        row("class-newInstance-again", () -> N.class.newInstance());
        row("unsafe-allocate", () -> {
            Field theUnsafe = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
            theUnsafe.setAccessible(true);
            return ((sun.misc.Unsafe) theUnsafe.get(null)).allocateInstance(U.class) != null;
        });
        row("ok-invoke", () -> Ok.class.getDeclaredMethod("seven").invoke(null));
        row("method-target-throws", () -> T.class.getDeclaredMethod("t").invoke(null));
        row("method-invoke-repeat", () -> C.class.getDeclaredMethod("m").invoke(null));
    }
}
