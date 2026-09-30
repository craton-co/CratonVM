// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 17, lane L4: a direct static-field getter /
// setter, a constructor handle and a static VarHandle on a class whose
// `<clinit>` throws raise the initialization error at their first use
// (`ExceptionInInitializerError`) and "Could not initialize class" at every
// later one (`NoClassDefFoundError`), exactly as `getstatic` / `putstatic` /
// `new` do. Before wave 17 CratonVM's handle arms swallowed the error: the
// getter and the VarHandle answered 0, the setter dropped its write and the
// constructor answered null ("returned ...").
//
// Run with the default settings and with --nojit; HotSpot 25 prints exactly:
//
//   getter#1 -> java.lang.ExceptionInInitializerError
//   getter#2 -> java.lang.NoClassDefFoundError
//   setter#1 -> java.lang.ExceptionInInitializerError
//   setter#2 -> java.lang.NoClassDefFoundError
//   ctor#1 -> java.lang.ExceptionInInitializerError
//   ctor#2 -> java.lang.NoClassDefFoundError
//   vh#1 -> java.lang.ExceptionInInitializerError
//   vh#2 -> java.lang.NoClassDefFoundError

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class HandleInitErrors {
    static boolean fail() {
        return Boolean.parseBoolean("true");
    }

    static class BadGet {
        static int f = 1;

        static {
            if (fail()) throw new IllegalStateException("boom");
        }
    }

    static class BadSet {
        static int f = 1;

        static {
            if (fail()) throw new IllegalStateException("boom");
        }
    }

    static class BadCtor {
        static {
            if (fail()) throw new IllegalStateException("boom");
        }

        BadCtor() {}
    }

    static class BadVh {
        static int f = 1;

        static {
            if (fail()) throw new IllegalStateException("boom");
        }
    }

    interface Act {
        Object run() throws Throwable;
    }

    static void attempt(String label, Act act) {
        try {
            Object r = act.run();
            System.out.println(label + " -> returned " + r);
        } catch (Throwable t) {
            System.out.println(label + " -> " + t.getClass().getName());
        }
    }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();

        MethodHandle getter = l.findStaticGetter(BadGet.class, "f", int.class);
        attempt("getter#1", () -> (Object) getter.invoke());
        attempt("getter#2", () -> (Object) getter.invoke());

        MethodHandle setter = l.findStaticSetter(BadSet.class, "f", int.class);
        attempt("setter#1", () -> {
            setter.invoke(7);
            return "void";
        });
        attempt("setter#2", () -> {
            setter.invoke(7);
            return "void";
        });

        MethodHandle ctor = l.findConstructor(BadCtor.class, MethodType.methodType(void.class));
        attempt("ctor#1", () -> ctor.invoke());
        attempt("ctor#2", () -> ctor.invoke());

        VarHandle vh = l.findStaticVarHandle(BadVh.class, "f", int.class);
        attempt("vh#1", () -> (Object) (int) vh.get());
        attempt("vh#2", () -> (Object) (int) vh.get());
    }
}
