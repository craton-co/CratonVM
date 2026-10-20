// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 84, lane L5 -- HotSpot `ClassFileParser`'s
// final-method-override and sealed-supertype refusals at define time
// (`classloading/src/class_manager.rs` `final_method_override_violation`,
// `sealed_permits`).
//
// Hand-written version-69 class files (every method `native`, so no Code
// attribute is needed) are defined through custom loaders, and impostors of
// this file's sealed types through a second loader and through
// `Lookup.defineClass` (same loader and package).
//
// CratonVM before wave 84: a user loader's class that overrides a final
// method was DEFINED (the structural pass is deferred for user loaders); an
// app-loader one failed with `VerifyError: X.m: cannot override final method
// <direct superclass>.m()V`; the sealed refusals read `class p/X is not a
// permitted subclass of sealed class p/S` and admitted a class of another
// loader named in PermittedSubclasses.
//
// Run:
//   $J/javac -d out L5W84FinalOverride.java
//   $J/java -cp out L5W84FinalOverride                       # HotSpot
//   $B --java-home $J/.. -cp out L5W84FinalOverride
//   $B --compatible --java-home $J/.. -cp out L5W84FinalOverride
//
// The expected block is read from JDK 25's classFileParser.cpp
// (`check_final_method_override`, `check_super_class_access`,
// `check_super_interface_access`, `InstanceKlass::has_as_permitted_subclass`).
// Sealed refusals carry `has_as_permitted_subclass`'s JDK 25 wording; a
// loader with an identity hash is cut at " @" for a stable line.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags, both CratonVM modes)
// override-final: java.lang.IncompatibleClassChangeError: class FoB overrides final method FoA.m()V
// override-final-two-up: java.lang.IncompatibleClassChangeError: class FoB2 overrides final method FoA.m()V
// package-private-other-package: defined fq2.D
// package-private-same-package: java.lang.IncompatibleClassChangeError: class fq1.F overrides final method fq1.C.n()V
// package-private-other-loader: defined fq1.E
// private-final: defined FoG
// sealed-class-same-loader: java.lang.IncompatibleClassChangeError: Failed listed permitted subclass check: class ImpC is not a permitted subclass of L5W84FinalOverride$SC
// sealed-interface-same-loader: java.lang.IncompatibleClassChangeError: Failed listed permitted subclass check: class ImpI is not a permitted subclass of L5W84FinalOverride$SI
// sealed-interface-extend: java.lang.IncompatibleClassChangeError: Failed listed permitted subclass check: class ImpJ is not a permitted subclass of L5W84FinalOverride$SI
// sealed-permitted-other-loader: java.lang.IncompatibleClassChangeError: Failed same module check: subclass L5W84FinalOverride$SCP2 is in module 'unnamed module' with loader L5W84FinalOverride$Loader @...
// sealed-same-package-check: java.lang.IncompatibleClassChangeError: Failed same package check: non-public subclass fs2.Sub is in package 'fs2' with classloader L5W84FinalOverride$Loader @...
// sealed-permitted-same-loader: ok L5W84FinalOverride$SCP
// HOTSPOT_EXPECTED_END
//
// The final-override rows were confirmed on Temurin 25.0.3 (ICCE); the
// sealed rows carry HotSpot 25's `has_as_permitted_subclass` wording as the
// coordinator measured it (the same-package row is derived from the same
// function and is still to be confirmed).

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public class L5W84FinalOverride {
    sealed interface SI permits SIP {}
    static final class SIP implements SI {}
    static sealed class SC permits SCP, SCP2 {}
    static final class SCP extends SC {}
    // Named in SC's PermittedSubclasses; the probe defines its own copy of
    // this name in ANOTHER loader (another unnamed module).
    static final class SCP2 extends SC {}

    static final class Loader extends ClassLoader {
        Loader(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    record Member(int flags, String name, String desc) {}

    /** A version-69 class file; every method is written without attributes. */
    static byte[] classFile(String thisName, int flags, String superName, String[] interfaces,
                            Member... methods) throws IOException {
        return classFile(thisName, flags, superName, interfaces, new String[0], methods);
    }

    /** As above, with a PermittedSubclasses attribute when `permitted` is not empty. */
    static byte[] classFile(String thisName, int flags, String superName, String[] interfaces,
                            String[] permitted, Member... methods) throws IOException {
        Map<String, Integer> utf8 = new LinkedHashMap<>();
        List<byte[]> pool = new ArrayList<>();
        java.util.function.Function<String, Integer> u = s -> utf8.computeIfAbsent(s, k -> {
            try {
                ByteArrayOutputStream e = new ByteArrayOutputStream();
                DataOutputStream d = new DataOutputStream(e);
                d.writeByte(1);
                d.writeUTF(k);
                pool.add(e.toByteArray());
            } catch (IOException ex) {
                throw new RuntimeException(ex);
            }
            return pool.size();
        });
        java.util.function.Function<String, Integer> cls = s -> {
            int name = u.apply(s);
            pool.add(new byte[] {7, (byte) (name >> 8), (byte) name});
            return pool.size();
        };
        int thisIdx = cls.apply(thisName);
        int superIdx = cls.apply(superName);
        int[] ifaceIdx = new int[interfaces.length];
        for (int i = 0; i < interfaces.length; i++) {
            ifaceIdx[i] = cls.apply(interfaces[i]);
        }
        int[][] m = new int[methods.length][];
        for (int i = 0; i < methods.length; i++) {
            m[i] = new int[] {methods[i].flags(), u.apply(methods[i].name()), u.apply(methods[i].desc())};
        }
        int[] permittedIdx = new int[permitted.length];
        for (int i = 0; i < permitted.length; i++) {
            permittedIdx[i] = cls.apply(permitted[i]);
        }
        int permittedName = permitted.length == 0 ? 0 : u.apply("PermittedSubclasses");
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bos);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(69);
        out.writeShort(pool.size() + 1);
        for (byte[] e : pool) {
            out.write(e);
        }
        out.writeShort(flags);
        out.writeShort(thisIdx);
        out.writeShort(superIdx);
        out.writeShort(ifaceIdx.length);
        for (int i : ifaceIdx) {
            out.writeShort(i);
        }
        out.writeShort(0); // fields
        out.writeShort(m.length);
        for (int[] x : m) {
            out.writeShort(x[0]);
            out.writeShort(x[1]);
            out.writeShort(x[2]);
            out.writeShort(0);
        }
        if (permitted.length == 0) {
            out.writeShort(0);
        } else {
            out.writeShort(1);
            out.writeShort(permittedName);
            out.writeInt(2 + 2 * permitted.length);
            out.writeShort(permitted.length);
            for (int i : permittedIdx) {
                out.writeShort(i);
            }
        }
        out.flush();
        return bos.toByteArray();
    }

    static final String OBJ = "java/lang/Object";
    static final String[] NONE = new String[0];

    interface Step {
        Class<?> run() throws Exception;
    }

    static void report(String label, Step step) {
        try {
            Class<?> c = step.run();
            System.out.println(label + ": defined " + c.getName());
        } catch (Throwable t) {
            // A user loader is printed with its identity hash: keep the
            // message up to the first " @" so the line is deterministic.
            String m = String.valueOf(t.getMessage());
            int at = m.indexOf(" @");
            if (at >= 0) {
                m = m.substring(0, at) + " @...";
            }
            System.out.println(label + ": " + t.getClass().getName() + ": " + m);
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W84FinalOverride.class.getClassLoader();
        Loader l = new Loader(app);
        // public final native void m(); public final native void n() is package-private below.
        l.define("FoA", classFile("FoA", 0x21, OBJ, NONE, new Member(0x0111, "m", "()V"),
                new Member(0x0112, "p", "()V")));
        report("override-final", () ->
                l.define("FoB", classFile("FoB", 0x21, "FoA", NONE, new Member(0x0101, "m", "()V"))));
        l.define("FoA2", classFile("FoA2", 0x21, "FoA", NONE));
        report("override-final-two-up", () ->
                l.define("FoB2", classFile("FoB2", 0x21, "FoA2", NONE, new Member(0x0101, "m", "()V"))));

        // fq1.C: public class, `final native void n()` package-private.
        l.define("fq1.C", classFile("fq1/C", 0x21, OBJ, NONE, new Member(0x0110, "n", "()V")));
        report("package-private-other-package", () ->
                l.define("fq2.D", classFile("fq2/D", 0x21, "fq1/C", NONE, new Member(0x0101, "n", "()V"))));
        report("package-private-same-package", () ->
                l.define("fq1.F", classFile("fq1/F", 0x21, "fq1/C", NONE, new Member(0x0101, "n", "()V"))));
        Loader child = new Loader(l);
        report("package-private-other-loader", () ->
                child.define("fq1.E", classFile("fq1/E", 0x21, "fq1/C", NONE, new Member(0x0101, "n", "()V"))));
        report("private-final", () ->
                l.define("FoG", classFile("FoG", 0x21, "FoA", NONE, new Member(0x0101, "p", "()V"))));

        String sc = "L5W84FinalOverride$SC";
        String si = "L5W84FinalOverride$SI";
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        report("sealed-class-same-loader", () ->
                lookup.defineClass(classFile("ImpC", 0x21, sc, NONE)));
        report("sealed-interface-same-loader", () ->
                lookup.defineClass(classFile("ImpI", 0x21, OBJ, new String[] {si})));
        report("sealed-interface-extend", () ->
                lookup.defineClass(classFile("ImpJ", 0x601, OBJ, new String[] {si})));
        Loader other = new Loader(app);
        report("sealed-permitted-other-loader", () ->
                other.define("L5W84FinalOverride$SCP2",
                        classFile("L5W84FinalOverride$SCP2", 0x30, sc, NONE)));
        // One user loader (one unnamed module): a NON-public listed subclass
        // in another package fails the package check.
        Loader seal = new Loader(app);
        seal.define("fs1.Seal", classFile("fs1/Seal", 0x21, OBJ, NONE, new String[] {"fs2/Sub"}));
        report("sealed-same-package-check", () ->
                seal.define("fs2.Sub", classFile("fs2/Sub", 0x30, "fs1/Seal", NONE)));
        System.out.println("sealed-permitted-same-loader: ok " + new SCP().getClass().getName());
    }
}
