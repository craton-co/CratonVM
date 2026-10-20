// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L4 (found and fixed in the wave's
// review): a `Lookup` field finder names a field that does not exist, on a
// class whose whole hierarchy declares NO fields (an interface such as
// `Runnable`, `Object`, an empty class). HotSpot's resolution refuses it
// (`NoSuchFieldException`); CratonVM's finders took "no field enumerated
// anywhere" as "cannot tell" (`mhn_field_resolve`, the compatibility-stub
// escape) and handed back a handle to nothing. `with-fields-getter` is the
// control that always matched.
//
// Run: javac -d out L4W49FieldlessClassMissingField.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W49FieldlessClassMissingField
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   iface-static-setter: java.lang.NoSuchFieldException
//   iface-static-getter: java.lang.NoSuchFieldException
//   object-getter: java.lang.NoSuchFieldException
//   empty-getter: java.lang.NoSuchFieldException
//   empty-static-var-handle: java.lang.NoSuchFieldException
//   empty-var-handle: java.lang.NoSuchFieldException
//   with-fields-getter: java.lang.NoSuchFieldException
import java.lang.invoke.MethodHandles;

public class L4W49FieldlessClassMissingField {
    static class Empty {
    }

    static class WithFields {
        int a;
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            Object o = body.run();
            out = "linked " + (o == null ? "null" : o.getClass().getSimpleName());
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        MethodHandles.Lookup l = MethodHandles.lookup();
        row("iface-static-setter", () -> l.findStaticSetter(Runnable.class, "nope", long.class));
        row("iface-static-getter", () -> l.findStaticGetter(Runnable.class, "nope", int.class));
        row("object-getter", () -> l.findGetter(Object.class, "nope", int.class));
        row("empty-getter", () -> l.findGetter(Empty.class, "nope", int.class));
        row("empty-static-var-handle", () -> l.findStaticVarHandle(Empty.class, "nope", int.class));
        row("empty-var-handle", () -> l.findVarHandle(Empty.class, "nope", int.class));
        row("with-fields-getter", () -> l.findGetter(WithFields.class, "nope", int.class));
    }
}
