// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 2, lane L3: an ACC_SYNCHRONIZED method must hold
// its monitor however the call arrives -- a plain call, reflection, or a
// method handle. `interpreter::execute` used to run the body without the
// monitor when a caller entered bytecode through it directly, so `notify()`
// on the monitor threw IllegalMonitorStateException.
//
// HotSpot 25 prints "ok" on every line (12 lines: 6 routes x 2 rounds), e.g.
//
//   direct instance: ok
//   direct static: ok
//   reflect instance: ok
//   reflect static: ok
//   handle instance: ok
//   handle static: ok
//
// Run under the default tiers and under --nojit.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class ExecuteSyncProbe {
    interface Call {
        String call() throws Throwable;
    }

    synchronized String inst() {
        notify(); // IllegalMonitorStateException unless `this` is locked
        return Thread.holdsLock(this) ? "ok" : "not-held";
    }

    static synchronized String stat() {
        ExecuteSyncProbe.class.notify();
        return Thread.holdsLock(ExecuteSyncProbe.class) ? "ok" : "not-held";
    }

    static String run(String label, Call c) {
        try {
            return label + ": " + c.call();
        } catch (Throwable t) {
            Throwable x = t instanceof InvocationTargetException ? t.getCause() : t;
            return label + ": " + x.getClass().getName();
        }
    }

    public static void main(String[] a) throws Throwable {
        ExecuteSyncProbe p = new ExecuteSyncProbe();
        Method mi = ExecuteSyncProbe.class.getDeclaredMethod("inst");
        Method ms = ExecuteSyncProbe.class.getDeclaredMethod("stat");
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle hi =
                lookup.findVirtual(ExecuteSyncProbe.class, "inst", MethodType.methodType(String.class));
        MethodHandle hs =
                lookup.findStatic(ExecuteSyncProbe.class, "stat", MethodType.methodType(String.class));
        for (int round = 0; round < 2; round++) {
            System.out.println(run("direct instance", p::inst));
            System.out.println(run("direct static", ExecuteSyncProbe::stat));
            System.out.println(run("reflect instance", () -> (String) mi.invoke(p)));
            System.out.println(run("reflect static", () -> (String) ms.invoke(null)));
            System.out.println(run("handle instance", () -> (String) hi.invokeExact(p)));
            System.out.println(run("handle static", () -> (String) hs.invokeExact()));
        }
    }
}
