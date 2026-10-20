// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L4 -- a static field handle named
// through a class that INHERITS the field (from a superinterface or a
// superclass). HotSpot resolves the field in its declaring class (JVMS
// 5.4.3.2) and the handle reads, writes and initializes that class. Under
// `--jdk-only` the JDK's own `StaticAccessor` asks `MethodHandleNatives
// .staticFieldOffset` / `staticFieldBase`, which looked the field up on the
// class the finder was asked on: `findStaticGetter(B.class, "IK", int.class)`
// of an interface constant raised `UnsupportedOperationException: real static
// field MethodHandle: unregistered offset` (`L4W24LookupFinderMessages`).
// The fixed row comes first and again at the end.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// init J
// iface-constant: 4
// iface-static: 7
// iface-static-set: java.lang.IllegalAccessException: unexpected set of a final field: L4W62InheritedStaticFieldHandles$J.JX/int/putStatic, from class L4W62InheritedStaticFieldHandles
// init A
// super-static: 5
// super-static-set: 6
// varhandle-iface: 4
// varhandle-super: 6
// b-initialized: false
// iface-constant-again: 4
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W62InheritedStaticFieldHandles`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W62InheritedStaticFieldHandles {
    static boolean bInit;

    static int init(String who, int v) {
        System.out.println("init " + who);
        return v;
    }

    interface J {
        int IK = 4;
        int JX = init("J", 7);
    }

    static class A {
        static int AX = init("A", 5);
    }

    static class B extends A implements J {
        static {
            bInit = true;
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        // The `from` tail names the unnamed module by its identity hash.
        int from = out.indexOf(" (unnamed module @");
        if (from >= 0) {
            out = out.substring(0, from);
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        row("iface-constant", () -> (int) l.findStaticGetter(B.class, "IK", int.class).invokeExact());
        MethodHandle jx = l.findStaticGetter(B.class, "JX", int.class);
        row("iface-static", () -> (int) jx.invokeExact());
        row("iface-static-set", () -> {
            MethodHandle set = l.findStaticSetter(B.class, "JX", int.class);
            return "unexpected " + set;
        });
        MethodHandle ax = l.findStaticGetter(B.class, "AX", int.class);
        row("super-static", () -> (int) ax.invokeExact());
        row("super-static-set", () -> {
            l.findStaticSetter(B.class, "AX", int.class).invokeExact(6);
            return A.AX;
        });
        row("varhandle-iface", () -> {
            VarHandle vh = l.findStaticVarHandle(B.class, "IK", int.class);
            return (int) vh.get();
        });
        row("varhandle-super", () -> {
            VarHandle vh = l.findStaticVarHandle(B.class, "AX", int.class);
            return (int) vh.get();
        });
        row("b-initialized", () -> bInit);
        row("iface-constant-again", () -> (int) l.findStaticGetter(B.class, "IK", int.class).invokeExact());
    }
}
