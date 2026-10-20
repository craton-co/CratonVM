// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L3 (review; the doors are lane L4's) --
// docs/internal/fixed-bugs/interpreter-L4-a-handle-door-s-class-init-failure-lists-no-jdk-frames-FIXED-20261023.md:
// a method handle and a VarHandle on a static member of a class whose static
// initializer throws. HotSpot initializes the class in the handle's own
// barrier (`DirectMethodHandle.ensureInitialized` /
// `LazyInitializingVarHandle.ensureInitialized`), under
// `Unsafe.ensureClassInitialized0`. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// mh-findStatic: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: c [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.internalMemberNameEnsureInit]
// vh-static-get: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: d [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.LazyInitializingVarHandle.initialize java.lang.invoke.LazyInitializingVarHandle.ensureInitialized java.lang.invoke.LazyInitializingVarHandle.target java.lang.invoke.VarHandleInts$FieldStaticReadOnly.get]
// mh-ok: 5
// mh-findStatic-again: java.lang.NoClassDefFoundError cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: c [in thread "main"] [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.internalMemberNameEnsureInit]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W58HandleClassInitFrames`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L3W58HandleClassInitFrames {
    static class C {
        static {
            if (true) {
                throw new IllegalStateException("c");
            }
        }

        static int m() {
            return 1;
        }
    }

    static class D {
        static int v = 4;
        static {
            if (true) {
                throw new IllegalStateException("d");
            }
        }
    }

    static class Ok {
        static int e() {
            return 5;
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

    public static void main(String[] args) throws Exception {
        MethodHandle c = MethodHandles.lookup().findStatic(C.class, "m", MethodType.methodType(int.class));
        VarHandle d = MethodHandles.lookup().findStaticVarHandle(D.class, "v", int.class);
        MethodHandle ok = MethodHandles.lookup().findStatic(Ok.class, "e", MethodType.methodType(int.class));
        row("mh-findStatic", () -> (int) c.invokeExact());
        row("vh-static-get", () -> (int) d.get());
        row("mh-ok", () -> (int) ok.invokeExact());
        row("mh-findStatic-again", () -> (int) c.invokeExact());
    }
}
