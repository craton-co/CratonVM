// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L4: the finders
// `L4W23LookupResolveErrors` does not cover, against HotSpot. CratonVM
// answers `MethodHandles.Lookup.find*` with its own natives in both modes
// (`native-builtins/src/lang_invoke.rs`, `register_p63_method_handles_lookup`),
// so each refusal's class AND message is theirs to get right: the JDK builds
// them in `MemberName.makeAccessException` as `<what>: <member>` where
// `<member>` is `MemberName.toString()` -- `Owner.name(ParamSimpleNames)Ret
// /refKind` for a method, `Owner.name/Type/refKind` for a field -- and, for a
// refusal `Lookup.checkField` / `checkAccess` raises, `, from <lookupClass>
// (<module>)`.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W24LookupFinderMessages
//
// HotSpot 25 (25.0.3) prints (`@x` masks the unnamed module's identity hash):
//   findVirtual JDK class missing: java.lang.NoSuchMethodException: no such method: java.lang.String.nope()void/invokeVirtual
//   findVirtual param display: java.lang.NoSuchMethodException: no such method: L4W24LookupFinderMessages$A.m(String,int[],Object[][])int/invokeVirtual
//   findStatic param display: java.lang.NoSuchMethodException: no such method: L4W24LookupFinderMessages$A.sm(long)int/invokeStatic
//   findSpecial missing: java.lang.NoSuchMethodException: no such method: L4W24LookupFinderMessages$A.nope()void/invokeSpecial
//   findConstructor display: java.lang.NoSuchMethodException: no such constructor: L4W24LookupFinderMessages$A.<init>(String,long[])void/newInvokeSpecial
//   findConstructor abstract: handle
//   findConstructor interface: java.lang.NoSuchMethodException: no such constructor: java.lang.Runnable.<init>()void/newInvokeSpecial
//   findConstructor inherited: java.lang.NoSuchMethodException: no such constructor: L4W24LookupFinderMessages$B.<init>(String,int[])void/newInvokeSpecial
//   findSetter missing: java.lang.NoSuchFieldException: no such field: L4W24LookupFinderMessages$A.nope/int/putField
//   findSetter wrong type: java.lang.NoSuchFieldException: no such field: L4W24LookupFinderMessages$A.f/java.lang.String/putField
//   findSetter final: java.lang.IllegalAccessException: unexpected set of a final field: L4W24LookupFinderMessages$A.ff/int/putField, from class L4W24LookupFinderMessages (unnamed module @x)
//   findSetter on static: java.lang.IllegalAccessException: expected a non-static field: L4W24LookupFinderMessages$A.sf/int/putStatic, from class L4W24LookupFinderMessages (unnamed module @x)
//   findStaticGetter missing: java.lang.NoSuchFieldException: no such field: L4W24LookupFinderMessages$A.nope/int/getStatic
//   findStaticSetter missing: java.lang.NoSuchFieldException: no such field: L4W24LookupFinderMessages$A.nope/int/putStatic
//   findStaticSetter final: java.lang.IllegalAccessException: unexpected set of a final field: L4W24LookupFinderMessages$A.SFF/int/putStatic, from class L4W24LookupFinderMessages (unnamed module @x)
//   findStaticSetter on instance: java.lang.IllegalAccessException: expected a static field: L4W24LookupFinderMessages$A.f/int/putField, from class L4W24LookupFinderMessages (unnamed module @x)
//   findGetter array type: java.lang.NoSuchFieldException: no such field: L4W24LookupFinderMessages$A.f/[Ljava.lang.String;/getField
//   findGetter inherited: 1
//   findStaticGetter interface constant via subclass: 4
//   findVarHandle missing: java.lang.NoSuchFieldException: no such field: L4W24LookupFinderMessages$A.nope/int/getField
//   findVarHandle wrong type: java.lang.NoSuchFieldException: no such field: L4W24LookupFinderMessages$A.f/long/getField
//   findVarHandle on static: java.lang.IllegalAccessException: expected a non-static field: L4W24LookupFinderMessages$A.sf/int/getStatic, from class L4W24LookupFinderMessages (unnamed module @x)
//   findStaticVarHandle missing: java.lang.NoSuchFieldException: no such field: L4W24LookupFinderMessages$A.nope/int/getStatic
//   findStaticVarHandle on instance: java.lang.IllegalAccessException: expected a static field: L4W24LookupFinderMessages$A.f/int/getField, from class L4W24LookupFinderMessages (unnamed module @x)
//   findSpecial null name: java.lang.NullPointerException
//   findSpecial null type: java.lang.NullPointerException
//   findVirtual ok: handle
//   findSetter ok: handle
//
// Before wave 24 (read from the code, not run): every refusal carried
// CratonVM's own text (`NoSuchMethodException: <internal/name>.m(desc)`,
// `NoSuchFieldException: A.f`, `IllegalAccessException: A.f: expected a
// static field` / `field is final`), `findConstructor inherited` answered a
// handle (existence was checked through the superclass chain), and
// `findStaticGetter missing`, every `findStaticSetter` row and the
// `findVarHandle wrong type` / `on static` and `findStaticVarHandle` rows
// answered a handle (nothing was checked); the two `findSpecial null` rows
// printed `java.lang.NoSuchMethodException`.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W24LookupFinderMessages {
    interface J { int IK = 4; }

    static class A {
        int f = 1;
        final int ff = 2;
        static int sf = 3;
        static final int SFF = 4;
        A() {}
        A(String s, int[] xs) {}
        void m(String s, int[] xs, Object[][] o) {}
        static long sm(long x) { return x; }
    }

    static abstract class Abs { Abs() {} }

    static class B extends A implements J { }

    interface Call { Object run() throws Throwable; }

    static void row(String name, Call c) {
        String out;
        try {
            Object r = c.run();
            out = r instanceof MethodHandle ? "handle"
                    : r instanceof VarHandle ? "varhandle"
                    : String.valueOf(r);
        } catch (Throwable t) {
            out = t.toString().replaceAll("@[0-9a-f]+", "@x");
        }
        System.out.println(name + ": " + out);
    }

    static void rowClass(String name, Call c) {
        String out;
        try {
            Object r = c.run();
            out = r instanceof MethodHandle ? "handle" : String.valueOf(r);
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType v = MethodType.methodType(void.class);
        row("findVirtual JDK class missing", () -> l.findVirtual(String.class, "nope", v));
        row("findVirtual param display", () -> l.findVirtual(A.class, "m",
                MethodType.methodType(int.class, String.class, int[].class, Object[][].class)));
        row("findStatic param display", () -> l.findStatic(A.class, "sm",
                MethodType.methodType(int.class, long.class)));
        row("findSpecial missing", () -> l.findSpecial(A.class, "nope", v, L4W24LookupFinderMessages.class));
        row("findConstructor display", () -> l.findConstructor(A.class,
                MethodType.methodType(void.class, String.class, long[].class)));
        row("findConstructor abstract", () -> l.findConstructor(Abs.class, v));
        row("findConstructor interface", () -> l.findConstructor(Runnable.class, v));
        row("findConstructor inherited", () -> l.findConstructor(B.class,
                MethodType.methodType(void.class, String.class, int[].class)));
        row("findSetter missing", () -> l.findSetter(A.class, "nope", int.class));
        row("findSetter wrong type", () -> l.findSetter(A.class, "f", String.class));
        row("findSetter final", () -> l.findSetter(A.class, "ff", int.class));
        row("findSetter on static", () -> l.findSetter(A.class, "sf", int.class));
        row("findStaticGetter missing", () -> l.findStaticGetter(A.class, "nope", int.class));
        row("findStaticSetter missing", () -> l.findStaticSetter(A.class, "nope", int.class));
        row("findStaticSetter final", () -> l.findStaticSetter(A.class, "SFF", int.class));
        row("findStaticSetter on instance", () -> l.findStaticSetter(A.class, "f", int.class));
        row("findGetter array type", () -> l.findGetter(A.class, "f", String[].class));
        row("findGetter inherited", () -> (int) l.findGetter(B.class, "f", int.class).invokeExact(new B()));
        row("findStaticGetter interface constant via subclass", () -> (int) l.findStaticGetter(B.class, "IK", int.class).invokeExact());
        row("findVarHandle missing", () -> l.findVarHandle(A.class, "nope", int.class));
        row("findVarHandle wrong type", () -> l.findVarHandle(A.class, "f", long.class));
        row("findVarHandle on static", () -> l.findVarHandle(A.class, "sf", int.class));
        row("findStaticVarHandle missing", () -> l.findStaticVarHandle(A.class, "nope", int.class));
        row("findStaticVarHandle on instance", () -> l.findStaticVarHandle(A.class, "f", int.class));
        // Exception CLASS only: HotSpot's helpful-NPE text names JDK locals.
        rowClass("findSpecial null name", () -> l.findSpecial(A.class, null, v, L4W24LookupFinderMessages.class));
        rowClass("findSpecial null type", () -> l.findSpecial(A.class, "m", null, L4W24LookupFinderMessages.class));
        row("findVirtual ok", () -> l.findVirtual(A.class, "m",
                MethodType.methodType(void.class, String.class, int[].class, Object[][].class)));
        row("findSetter ok", () -> l.findSetter(A.class, "f", int.class));
    }
}
