// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L4: what `MethodHandles.Lookup`'s
// finders throw for a member that does not exist, exists with the other
// static-ness, exists with another type, or is not accessible — the
// exception class and its message, which `MemberName.makeAccessException`
// builds from the `LinkageError` that `MethodHandleNatives.resolve` raised
// (NoSuchMethodError / NoSuchFieldError -> NoSuchMethodException /
// NoSuchFieldException; IncompatibleClassChangeError / IllegalAccessError ->
// IllegalAccessException). A resolve that returns an unresolvable member as
// resolved hands back a handle instead ("handle" rows below), and the error
// surfaces at the call, if at all.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W23LookupResolveErrors
//
// HotSpot 25 (25.0.3) prints:
//   findStatic missing: java.lang.NoSuchMethodException: no such method: L4W23LookupResolveErrors$T.nope()void/invokeStatic
//   findVirtual missing: java.lang.NoSuchMethodException: no such method: L4W23LookupResolveErrors$T.nope()void/invokeVirtual
//   findVirtual wrong return: java.lang.NoSuchMethodException: no such method: L4W23LookupResolveErrors$T.inst()int/invokeVirtual
//   findVirtual on static: java.lang.IllegalAccessException: no such method: L4W23LookupResolveErrors$T.stat()void/invokeVirtual
//   findStatic on instance: java.lang.IllegalAccessException: no such method: L4W23LookupResolveErrors$T.inst()void/invokeStatic
//   findConstructor missing: java.lang.NoSuchMethodException: no such constructor: L4W23LookupResolveErrors$T.<init>(int)void/newInvokeSpecial
//   findConstructor only in superclass: java.lang.NoSuchMethodException: no such constructor: L4W23LookupResolveErrors$Q.<init>(int)void/newInvokeSpecial
//   findGetter missing: java.lang.NoSuchFieldException: no such field: L4W23LookupResolveErrors$T.nope/int/getField
//   findGetter wrong type: java.lang.NoSuchFieldException: no such field: L4W23LookupResolveErrors$T.f/long/getField
//   findGetter on static: java.lang.IllegalAccessException: expected a non-static field: L4W23LookupResolveErrors$T.sf/int/getStatic, from class L4W23LookupResolveErrors (unnamed module @x)
//   findStaticGetter on instance: java.lang.IllegalAccessException: expected a static field: L4W23LookupResolveErrors$T.f/int/getField, from class L4W23LookupResolveErrors (unnamed module @x)
//   findVirtual private other: java.lang.IllegalAccessException: symbolic reference class is not accessible: class L4W23LookupResolveErrors$T, from public Lookup
//   findVirtual interface missing: java.lang.NoSuchMethodException: no such method: java.lang.Runnable.nope()void/invokeInterface
//   findVirtual default via class: 11
//   findStatic interface static via class: java.lang.NoSuchMethodException: no such method: L4W23LookupResolveErrors$T.ism()int/invokeStatic
//   findStaticGetter interface constant via class: 5
//   findVirtual Object method via interface: true
//   findVirtual inherited Object method: false
//   findVirtual ok: 7
//   findStatic ok: 8
//   findGetter ok: 9
//
// The `@x` is the module's identity hash, masked. Two rows are worth a note:
// the static-ness of a FIELD is not a resolution error (HotSpot resolves the
// field and rewrites the reference kind to the field's own, `getStatic` /
// `getField`, which `Lookup.checkField` then refuses — hence the kind in the
// message), while a METHOD of the other static-ness is
// (`IncompatibleClassChangeError` from `LinkResolver`, reported as
// `IllegalAccessException: no such method`). A static interface method is
// not inherited by an implementing class (JVMS 5.4.3.3), an interface
// constant is.
//
// Wave 24 (lane L4): the wave-23 Linux run printed these rows UNCHANGED in
// both modes, because `Lookup.find*` never reaches `MethodHandleNatives
// .resolve` on CratonVM -- `native-builtins/src/lang_invoke.rs`'s own finder
// natives answer it (`lookup_find_virtual` & co.). They printed CratonVM's
// texts (`NoSuchMethodException: L4W23LookupResolveErrors$T.nope()V`,
// `IllegalAccessException: ...T.stat()V: expected a non-static method`,
// `NoSuchFieldException: L4W23LookupResolveErrors$T.nope`), `handle` for
// `findConstructor only in superclass`, `java/lang/Runnable.nope()V` for
// `findVirtual interface missing`, and nothing from `findVirtual default via
// class` on (the run stopped there). Wave 24 rebuilt the finders' refusals on
// the wave-23 resolution searches; see also `L4W24LookupFinderMessages`.
//
// What CratonVM printed before wave 23, read from the code, not run:
// `native_mhn_resolve` matched no member by type, threw nothing, and read
// access flags from the superclass chain only. So a missing or wrong-typed
// member came back "resolved" with no flags: `handle` for the virtual,
// getter and constructor rows, and `IllegalAccessException: expected a
// static method` / `expected a static field` where the finder wants a
// static (the missing flag reads as non-static) — including the interface
// constant, which exists. The two method static-ness rows said `expected a
// (non-)static method`, and the two field static-ness rows kept the
// requested reference kind in the message.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W23LookupResolveErrors {
    interface I {
        int K = 5;
        default int dm() { return 11; }
        static int ism() { return 12; }
    }

    static class T implements I {
        int f = 9;
        static int sf = 3;
        T() {}
        static void stat() {}
        void inst() {}
        int seven() { return 7; }
        static int eight() { return 8; }
        private void priv() {}
    }

    static class P { P(int x) {} }
    static class Q extends P { Q() { super(1); } }

    interface Call { Object run() throws Throwable; }

    static void row(String name, Call c) {
        String out;
        try {
            Object r = c.run();
            out = r instanceof MethodHandle ? "handle" : String.valueOf(r);
        } catch (Throwable t) {
            out = t.toString().replaceAll("@[0-9a-f]+", "@x");
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType v = MethodType.methodType(void.class);
        row("findStatic missing", () -> l.findStatic(T.class, "nope", v));
        row("findVirtual missing", () -> l.findVirtual(T.class, "nope", v));
        row("findVirtual wrong return", () -> l.findVirtual(T.class, "inst", MethodType.methodType(int.class)));
        row("findVirtual on static", () -> l.findVirtual(T.class, "stat", v));
        row("findStatic on instance", () -> l.findStatic(T.class, "inst", v));
        row("findConstructor missing", () -> l.findConstructor(T.class, MethodType.methodType(void.class, int.class)));
        row("findConstructor only in superclass", () -> l.findConstructor(Q.class, MethodType.methodType(void.class, int.class)));
        row("findGetter missing", () -> l.findGetter(T.class, "nope", int.class));
        row("findGetter wrong type", () -> l.findGetter(T.class, "f", long.class));
        row("findGetter on static", () -> l.findGetter(T.class, "sf", int.class));
        row("findStaticGetter on instance", () -> l.findStaticGetter(T.class, "f", int.class));
        row("findVirtual private other", () -> MethodHandles.publicLookup().findVirtual(T.class, "priv", v));
        row("findVirtual interface missing", () -> l.findVirtual(Runnable.class, "nope", v));
        row("findVirtual default via class", () -> (int) l.findVirtual(T.class, "dm", MethodType.methodType(int.class)).invokeExact(new T()));
        row("findStatic interface static via class", () -> l.findStatic(T.class, "ism", MethodType.methodType(int.class)));
        row("findStaticGetter interface constant via class", () -> (int) l.findStaticGetter(T.class, "K", int.class).invokeExact());
        row("findVirtual Object method via interface", () -> (String) l.findVirtual(Runnable.class, "toString", MethodType.methodType(String.class)).invokeExact((Runnable) () -> {}) != null);
        row("findVirtual inherited Object method", () -> (boolean) l.findVirtual(T.class, "equals", MethodType.methodType(boolean.class, Object.class)).invokeExact(new T(), (Object) null));
        row("findVirtual ok", () -> (int) l.findVirtual(T.class, "seven", MethodType.methodType(int.class)).invokeExact(new T()));
        row("findStatic ok", () -> (int) l.findStatic(T.class, "eight", MethodType.methodType(int.class)).invokeExact());
        row("findGetter ok", () -> (int) l.findGetter(T.class, "f", int.class).invokeExact(new T()));
    }
}
