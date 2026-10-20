// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L4 (review): `MethodHandles.Lookup.in`
// across modules ("teleporting") and `dropLookupMode`. JDK 25 `Lookup.in`
// records the lookup class it teleported FROM as the new lookup's
// `previousLookupClass()` when the target is in another module, drops MODULE,
// PACKAGE, PRIVATE and PROTECTED, and answers 0 modes for a hop to a THIRD
// module; `toString()` then names both classes.
//
// Run: javac -d out L4W43LookupTeleport.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W43LookupTeleport
//
// Expected HotSpot 25 output (default and -Xint, measured locally), the same
// in --compatible:
//   self: L4W43LookupTeleport prev=null modes=95 str=L4W43LookupTeleport
//   in-own: true
//   in-nested: L4W43LookupTeleport$Nested prev=null modes=31 str=L4W43LookupTeleport$Nested
//   in-string: java.lang.String prev=L4W43LookupTeleport modes=1 str=java.lang.String/L4W43LookupTeleport/public
//   in-string-back: L4W43LookupTeleport prev=java.lang.String modes=0 str=L4W43LookupTeleport/java.lang.String/noaccess
//   in-string-object: java.lang.Object prev=L4W43LookupTeleport modes=1 str=java.lang.Object/L4W43LookupTeleport/public
//   in-string-third: java.sql.Connection prev=java.lang.String modes=0 str=java.sql.Connection/java.lang.String/noaccess
//   in-string-find: 7
//   public-in: java.lang.String prev=null modes=32 str=java.lang.String/publicLookup
//   drop-private: L4W43LookupTeleport prev=null modes=25 str=L4W43LookupTeleport/package
//   drop-module: L4W43LookupTeleport prev=null modes=1 str=L4W43LookupTeleport/public
//   drop-public: L4W43LookupTeleport prev=null modes=0 str=L4W43LookupTeleport/noaccess
//   drop-original: L4W43LookupTeleport prev=null modes=27 str=L4W43LookupTeleport/private
//   drop-unconditional: L4W43LookupTeleport prev=null modes=27 str=L4W43LookupTeleport/private
//   drop-bad: java.lang.IllegalArgumentException: 320 is not a valid mode to drop
//   teleported-drop: java.lang.String prev=L4W43LookupTeleport modes=0 str=java.lang.String/L4W43LookupTeleport/noaccess
//   same-module-public: pb.B prev=null modes=17 str=pb.B/module
//   same-module-package-private: pb.C prev=null modes=0 str=pb.C/noaccess
//   in-null: java.lang.NullPointerException: null
//   in-int: java.lang.IllegalArgumentException: int is a primitive class
//   in-array: java.lang.IllegalArgumentException: class [Ljava.lang.String; is an array class
//
// Read from the code, CratonVM before wave 43: `in-own` false (a copy, not
// `this`); every `prev=` null, since the `in` native never wrote
// `prevLookupClass` (so `str=` never names a second class, and a
// `teleported-drop` keeps no previous class); `in-string-back` modes 1 (a
// package-private class of another package was not refused);
// `same-module-public` modes 1 (MODULE dropped for another package of the
// same module) and `same-module-package-private` modes 1; `in-null`'s
// message was CratonVM's own (`Lookup.in: requestedLookupClass is null`;
// the CROSS-LANE commit in `classloader.rs`). The cross-module rows need the module registry: without it
// (`java.base` not registered) `in` keeps its package-based reductions and
// writes a null `prevLookupClass`.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;

public class L4W43LookupTeleport {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static String show(Lookup l) {
        Class<?> prev = l.previousLookupClass();
        return l.lookupClass().getName() + " prev=" + (prev == null ? "null" : prev.getName()) + " modes="
                + l.lookupModes() + " str=" + l;
    }

    static class Nested {
    }

    /**
     * `pa.A.run()` answers `MethodHandles.lookup().in(pb.B.class)`: two
     * packages of ONE module (the unnamed module of this loader). `pb.B` is
     * public; `pb.C` is not.
     */
    static Lookup otherPackage(String target) throws Throwable {
        ClassDesc lk = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
        ClassLoader loader = new ClassLoader(L4W43LookupTeleport.class.getClassLoader()) {
            @Override
            protected Class<?> findClass(String name) throws ClassNotFoundException {
                byte[] b;
                if (name.equals("pa.A")) {
                    b = ClassFile.of().build(ClassDesc.of("pa.A"), cb -> {
                        cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
                        cb.withMethodBody("run", MethodTypeDesc.of(lk, ConstantDescs.CD_Class),
                                ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code
                                        .invokestatic(ClassDesc.of("java.lang.invoke.MethodHandles"), "lookup",
                                                MethodTypeDesc.of(lk))
                                        .aload(0)
                                        .invokevirtual(lk, "in", MethodTypeDesc.of(lk, ConstantDescs.CD_Class))
                                        .areturn());
                    });
                } else if (name.equals("pb.B") || name.equals("pb.C")) {
                    b = ClassFile.of().build(ClassDesc.of(name), cb -> cb.withFlags(
                            (name.equals("pb.B") ? ClassFile.ACC_PUBLIC : 0) | ClassFile.ACC_SUPER));
                } else {
                    throw new ClassNotFoundException(name);
                }
                return defineClass(name, b, 0, b.length);
            }
        };
        Class<?> a = loader.loadClass("pa.A");
        return (Lookup) a.getMethod("run", Class.class).invoke(null, loader.loadClass(target));
    }

    public static void main(String[] args) throws Throwable {
        Lookup self = MethodHandles.lookup();
        row("self", () -> show(self));
        row("in-own", () -> self.in(L4W43LookupTeleport.class) == self);
        row("in-nested", () -> show(self.in(Nested.class)));
        row("in-string", () -> show(self.in(String.class)));
        row("in-string-back", () -> show(self.in(String.class).in(L4W43LookupTeleport.class)));
        row("in-string-object", () -> show(self.in(String.class).in(Object.class)));
        row("in-string-third", () -> show(self.in(String.class).in(java.sql.Connection.class)));
        row("in-string-find", () -> self.in(String.class).findStatic(String.class, "valueOf",
                java.lang.invoke.MethodType.methodType(String.class, int.class)).invoke(7));
        row("public-in", () -> show(MethodHandles.publicLookup().in(String.class)));
        row("drop-private", () -> show(self.dropLookupMode(Lookup.PRIVATE)));
        row("drop-module", () -> show(self.dropLookupMode(Lookup.MODULE)));
        row("drop-public", () -> show(self.dropLookupMode(Lookup.PUBLIC)));
        row("drop-original", () -> show(self.dropLookupMode(Lookup.ORIGINAL)));
        row("drop-unconditional", () -> show(self.dropLookupMode(Lookup.UNCONDITIONAL)));
        row("drop-bad", () -> show(self.dropLookupMode(0x40 | 0x100)));
        row("teleported-drop", () -> show(self.in(String.class).dropLookupMode(Lookup.PUBLIC)));
        row("same-module-public", () -> show(otherPackage("pb.B")));
        row("same-module-package-private", () -> show(otherPackage("pb.C")));
        row("in-null", () -> self.in(null));
        row("in-int", () -> self.in(int.class));
        row("in-array", () -> self.in(String[].class));
    }
}
