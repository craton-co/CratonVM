// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 5, lane L3: a null `this` reaching a synchronized
// instance method through the recursive entry doors (reflection, method
// handles) must surface as a catchable NullPointerException, never as a
// VM-internal error that no Java `catch` sees. `interpreter::execute` and
// `invoke_on_class_shared_inner`'s monitor helper used to answer
// "synchronized instance method called with null or missing this" as an
// internal failure.
//
// Plain probe: no flags, no setup. Only exception CLASS names are printed (the
// JEP 358 text of an NPE raised inside JDK reflection code is JDK-internal).
// HotSpot 25 prints exactly:
//
//   reflect-sync: java.lang.NullPointerException
//   reflect-plain: java.lang.NullPointerException
//   mh-sync: java.lang.NullPointerException
//   mh-plain: java.lang.NullPointerException
//   caught-in-java: java.lang.NullPointerException
//   ok-sync: 42
//   ok-static-sync: 7
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class NullThisSyncProbe {
    int value = 42;

    public synchronized int sync() {
        return value;
    }

    public int plain() {
        return value;
    }

    public static synchronized int staticSync() {
        return 7;
    }

    static String outcome(Throwable t) {
        Throwable c = t;
        while (c instanceof InvocationTargetException && c.getCause() != null) {
            c = c.getCause();
        }
        return c.getClass().getName();
    }

    static void reflect(String label, String name) {
        try {
            Method m = NullThisSyncProbe.class.getMethod(name);
            Object r = m.invoke(null);
            System.out.println(label + ": returned " + r);
        } catch (Throwable t) {
            System.out.println(label + ": " + outcome(t));
        }
    }

    static void handle(String label, String name) {
        try {
            MethodHandle mh = MethodHandles.lookup()
                    .findVirtual(NullThisSyncProbe.class, name, MethodType.methodType(int.class));
            int r = (int) mh.invokeExact((NullThisSyncProbe) null);
            System.out.println(label + ": returned " + r);
        } catch (Throwable t) {
            System.out.println(label + ": " + outcome(t));
        }
    }

    public static void main(String[] args) throws Throwable {
        reflect("reflect-sync", "sync");
        reflect("reflect-plain", "plain");
        handle("mh-sync", "sync");
        handle("mh-plain", "plain");
        // The same NPE must be catchable from Java around the call.
        try {
            NullThisSyncProbe p = null;
            p.sync();
            System.out.println("caught-in-java: no exception");
        } catch (NullPointerException e) {
            System.out.println("caught-in-java: " + e.getClass().getName());
        }
        MethodHandle ok = MethodHandles.lookup()
                .findVirtual(NullThisSyncProbe.class, "sync", MethodType.methodType(int.class));
        System.out.println("ok-sync: " + (int) ok.invokeExact(new NullThisSyncProbe()));
        Method st = NullThisSyncProbe.class.getMethod("staticSync");
        System.out.println("ok-static-sync: " + st.invoke(null));
    }
}
