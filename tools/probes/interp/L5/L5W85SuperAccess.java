// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 85, lane L5 -- HotSpot `ClassFileParser`'s
// `check_super_class_access` / `check_super_interface_access`: a class may
// not be defined over a direct supertype it cannot access
// (`classloading/src/class_manager.rs` `supertype_link_refusal`,
// `super_access_refusal`; `access_control::super_access_denied_message`).
//
// Hand-written version-69 class files (no methods) are defined through
// custom loaders and through `Lookup.defineClass` (the application loader).
// CratonVM before wave 85 defined every one of them.
//
// Identity hashes are not deterministic: every `unnamed module @0x<hex>` is
// printed `unnamed module` and every other ` @<hex>` ` @?`.
//
// Run:
//   $J/javac -d out L5W85SuperAccess.java
//   $J/java -cp out L5W85SuperAccess                       # HotSpot
//   $B --java-home $J/.. -cp out L5W85SuperAccess
//   $B --compatible --java-home $J/.. -cp out L5W85SuperAccess
// The expected block is read from JDK 25's classFileParser.cpp and
// reflection.cpp (`verify_class_access_msg`) and `Klass::
// joint_in_module_of_loader` / `class_in_module_of_loader`; confirm it on the
// host first.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags, both CratonVM modes)
// package-private-other-package: java.lang.IllegalAccessError: class fa2.Sub cannot access its superclass fa1.Base (fa2.Sub and fa1.Base are in unnamed module of loader L5W85SuperAccess$Loader @?)
// abstract-superclass: java.lang.IllegalAccessError: class fa2.Sub2 cannot access its abstract superclass fa1.AbsBase (fa2.Sub2 and fa1.AbsBase are in unnamed module of loader L5W85SuperAccess$Loader @?)
// other-loader-same-package: java.lang.IllegalAccessError: class fa1.Sub3 cannot access its superclass fa1.Base (fa1.Sub3 is in unnamed module of loader L5W85SuperAccess$Loader @?; fa1.Base is in unnamed module of loader L5W85SuperAccess$Loader @?)
// package-private-interface: java.lang.IllegalAccessError: class fa2.Impl cannot access its superinterface fa1.Hidden (fa2.Impl and fa1.Hidden are in unnamed module of loader L5W85SuperAccess$Loader @?)
// last-interface-first: java.lang.IllegalAccessError: class fa2.Impl2 cannot access its superinterface fa1.Hidden2 (fa2.Impl2 and fa1.Hidden2 are in unnamed module of loader L5W85SuperAccess$Loader @?)
// not-exported-superclass: java.lang.IllegalAccessError: superclass access check failed: class fa2.Leaky (in unnamed module) cannot access class jdk.internal.loader.BuiltinClassLoader (in module java.base) because module java.base does not export jdk.internal.loader to unnamed module
// not-exported-interface: java.lang.IllegalAccessError: superinterface check failed: class fa2.Leaky2 (in unnamed module) cannot access class jdk.internal.access.JavaLangAccess (in module java.base) because module java.base does not export jdk.internal.access to unnamed module
// app-loader-jdk-interface: java.lang.IllegalAccessError: class AppImpl cannot access its superinterface java.util.stream.Sink (AppImpl is in unnamed module of loader 'app'; java.util.stream.Sink is in module java.base of loader 'bootstrap')
// public-superclass: defined fa2.Ok
// same-package: defined fa1.Near
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public class L5W85SuperAccess {
    static final class Loader extends ClassLoader {
        Loader(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /** A version-69 class file with no fields, methods or attributes. */
    static byte[] classFile(String thisName, int flags, String superName, String... interfaces)
            throws IOException {
        Map<String, Integer> classes = new LinkedHashMap<>();
        List<byte[]> pool = new ArrayList<>();
        java.util.function.Function<String, Integer> cls = s -> classes.computeIfAbsent(s, k -> {
            try {
                ByteArrayOutputStream e = new ByteArrayOutputStream();
                DataOutputStream d = new DataOutputStream(e);
                d.writeByte(1);
                d.writeUTF(k);
                pool.add(e.toByteArray());
                int utf = pool.size();
                pool.add(new byte[] {7, (byte) (utf >> 8), (byte) utf});
                return pool.size();
            } catch (IOException ex) {
                throw new RuntimeException(ex);
            }
        });
        int thisIdx = cls.apply(thisName);
        int superIdx = cls.apply(superName);
        int[] ifaceIdx = new int[interfaces.length];
        for (int i = 0; i < interfaces.length; i++) {
            ifaceIdx[i] = cls.apply(interfaces[i]);
        }
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
        out.writeShort(0); // methods
        out.writeShort(0); // attributes
        out.flush();
        return bos.toByteArray();
    }

    static final String OBJ = "java/lang/Object";

    interface Step {
        Class<?> run() throws Exception;
    }

    static String stable(String m) {
        return String.valueOf(m)
                .replaceAll("unnamed module @0x[0-9a-fA-F]+", "unnamed module")
                .replaceAll(" @[0-9a-fA-F]+", " @?");
    }

    static void report(String label, Step step) {
        try {
            Class<?> c = step.run();
            System.out.println(label + ": defined " + c.getName());
        } catch (Throwable t) {
            System.out.println(label + ": " + t.getClass().getName() + ": " + stable(t.getMessage()));
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W85SuperAccess.class.getClassLoader();
        Loader l = new Loader(app);
        l.define("fa1.Base", classFile("fa1/Base", 0x20, OBJ));
        l.define("fa1.AbsBase", classFile("fa1/AbsBase", 0x420, OBJ));
        l.define("fa1.PubBase", classFile("fa1/PubBase", 0x21, OBJ));
        l.define("fa1.Hidden", classFile("fa1/Hidden", 0x600, OBJ));
        l.define("fa1.Hidden2", classFile("fa1/Hidden2", 0x600, OBJ));

        report("package-private-other-package", () ->
                l.define("fa2.Sub", classFile("fa2/Sub", 0x21, "fa1/Base")));
        report("abstract-superclass", () ->
                l.define("fa2.Sub2", classFile("fa2/Sub2", 0x21, "fa1/AbsBase")));
        Loader child = new Loader(l);
        report("other-loader-same-package", () ->
                child.define("fa1.Sub3", classFile("fa1/Sub3", 0x21, "fa1/Base")));
        report("package-private-interface", () ->
                l.define("fa2.Impl", classFile("fa2/Impl", 0x21, OBJ, "fa1/Hidden")));
        report("last-interface-first", () ->
                l.define("fa2.Impl2", classFile("fa2/Impl2", 0x21, OBJ, "fa1/Hidden", "fa1/Hidden2")));
        report("not-exported-superclass", () ->
                l.define("fa2.Leaky", classFile("fa2/Leaky", 0x21, "jdk/internal/loader/BuiltinClassLoader")));
        report("not-exported-interface", () ->
                l.define("fa2.Leaky2", classFile("fa2/Leaky2", 0x21, OBJ, "jdk/internal/access/JavaLangAccess")));
        report("app-loader-jdk-interface", () ->
                MethodHandles.lookup().defineClass(classFile("AppImpl", 0x21, OBJ, "java/util/stream/Sink")));
        report("public-superclass", () ->
                l.define("fa2.Ok", classFile("fa2/Ok", 0x21, "fa1/PubBase")));
        report("same-package", () ->
                l.define("fa1.Near", classFile("fa1/Near", 0x21, "fa1/Base", "fa1/Hidden")));
    }
}
