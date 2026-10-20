// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 95, lane L5 -- `findLoadedClass` of a class whose
// define a sealed package refused answers null, as on HotSpot, where the class
// was never defined (`BuiltinClassLoader.defineClass(cn, Resource)` ->
// `defineOrCheckPackage` -> `getAndVerifyPackage` throws before
// `defineClass1`). The VM defines class-path classes itself; after a refused
// door (wave 94, `L5W94SealedUnlinkedDoors`) it held `B` in `Loaded` and its
// `findLoadedClass` natives handed it out
// (`interpreter-i92-L5-unlinked-class-path-class-doors-skip-the-package-step`,
// "Still open").
//
// `setup <dir>` writes three JARs:
//   a.jar  manifest `Sealed: true`, class `l5w95seal.A` (public, no members)
//   b.jar  class `l5w95seal.B`, no manifest
//   c.jar  class `l5w95use.Use`, built with java.lang.classfile: static
//          `ldc()` (`ldc l5w95seal/B`)
//
// The package is sealed by loading `A`; `B` is then refused at `ldc`, at
// `Class.forName(n, false, app)` and at `loadClass`, and `findLoadedClass` is
// asked (through reflection) between the doors.
//
// SETUP and run (one directory; `;` for `:` on Windows):
//   javac -d out L5W95FindLoadedSealedRefused.java
//   java -cp out L5W95FindLoadedSealedRefused setup .
//   java --add-opens java.base/java.lang=ALL-UNNAMED -cp out:a.jar:b.jar:c.jar L5W95FindLoadedSealedRefused
//   cratonvm [--compatible] --add-opens java.base/java.lang=ALL-UNNAMED -cp out:a.jar:b.jar:c.jar L5W95FindLoadedSealedRefused
//   (`CRATONVM_UNLINKED_DOORS_CHECK_PACKAGE=0` turns the doors' step and this
//   answer off: `find-B-*` then print `l5w95seal.B` after the first door.)
//
// HOTSPOT_EXPECTED (derived from the JDK 25 sources, not run: `B` is never
// defined, so `findLoadedClass0` finds nothing for it; `A` was defined by the
// application loader; `Method.invoke` wraps the door's exception in
// `InvocationTargetException`, whose cause is printed):
//   a=l5w95seal.A
//   find-A=l5w95seal.A
//   find-B-0=null
//   ldc=java.lang.SecurityException msg=sealing violation: package l5w95seal is sealed
//   find-B-1=null
//   forname=java.lang.SecurityException msg=sealing violation: package l5w95seal is sealed
//   find-B-2=null
//   load=java.lang.SecurityException msg=sealing violation: package l5w95seal is sealed
//   find-B-3=null
//   find-C=null

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.concurrent.Callable;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L5W95FindLoadedSealedRefused {
    static String attempt(Callable<Object> c) {
        try {
            Object v = c.call();
            return v instanceof Class<?> k ? k.getName() : String.valueOf(v);
        } catch (Throwable t) {
            if (t instanceof InvocationTargetException ite && ite.getCause() != null) {
                t = ite.getCause();
            }
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    /** A public class `internal` extending Object, with no members. */
    static byte[] emptyClass(String internal) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(5);
        out.writeByte(7);
        out.writeShort(2);
        out.writeByte(1);
        out.writeUTF(internal);
        out.writeByte(7);
        out.writeShort(4);
        out.writeByte(1);
        out.writeUTF("java/lang/Object");
        out.writeShort(0x0021);
        out.writeShort(1);
        out.writeShort(3);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    static byte[] useClass() {
        ClassDesc b = ClassDesc.of("l5w95seal.B");
        int flags = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        MethodTypeDesc toObject = MethodTypeDesc.of(ConstantDescs.CD_Object);
        return ClassFile.of().build(ClassDesc.of("l5w95use.Use"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("ldc", toObject, flags, code -> code.ldc(b).areturn());
        });
    }

    static void setup(File dir) throws IOException {
        Manifest man = new Manifest();
        man.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        man.getMainAttributes().put(Attributes.Name.SEALED, "true");
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(new File(dir, "a.jar")), man)) {
            out.putNextEntry(new JarEntry("l5w95seal/A.class"));
            out.write(emptyClass("l5w95seal/A"));
            out.closeEntry();
        }
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(new File(dir, "b.jar")))) {
            out.putNextEntry(new JarEntry("l5w95seal/B.class"));
            out.write(emptyClass("l5w95seal/B"));
            out.closeEntry();
            out.putNextEntry(new JarEntry("l5w95seal/C.class"));
            out.write(emptyClass("l5w95seal/C"));
            out.closeEntry();
        }
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(new File(dir, "c.jar")))) {
            out.putNextEntry(new JarEntry("l5w95use/Use.class"));
            out.write(useClass());
            out.closeEntry();
        }
        System.out.println("wrote a.jar, b.jar and c.jar in " + dir);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("setup")) {
            setup(new File(args[1]));
            return;
        }
        // The probe battery runs every W9x probe as `-cp .` with no setup and no
        // `--add-opens`: there `setAccessible` threw `InaccessibleObjectException`,
        // whose message names `unnamed module @<identity hash>` (different on every
        // run, HotSpot included), so the battery row never matched (wave 103).
        // Without its setup the probe now prints one deterministic line instead.
        boolean opened = Object.class.getModule()
                .isOpen("java.lang", L5W95FindLoadedSealedRefused.class.getModule());
        boolean jars = ClassLoader.getSystemResource("l5w95seal/A.class") != null
                && ClassLoader.getSystemResource("l5w95use/Use.class") != null;
        if (!opened || !jars) {
            System.out.println("not-set-up opened=" + opened + " jars=" + jars
                    + " (see the header: setup, then --add-opens java.base/java.lang=ALL-UNNAMED)");
            return;
        }
        ClassLoader app = ClassLoader.getSystemClassLoader();
        Method find = ClassLoader.class.getDeclaredMethod("findLoadedClass", String.class);
        find.setAccessible(true);
        System.out.println("a=" + attempt(() -> Class.forName("l5w95seal.A", false, app)));
        System.out.println("find-A=" + attempt(() -> find.invoke(app, "l5w95seal.A")));
        System.out.println("find-B-0=" + attempt(() -> find.invoke(app, "l5w95seal.B")));
        Class<?> use = Class.forName("l5w95use.Use", true, app);
        Method ldc = use.getMethod("ldc");
        System.out.println("ldc=" + attempt(() -> ldc.invoke(null)));
        System.out.println("find-B-1=" + attempt(() -> find.invoke(app, "l5w95seal.B")));
        System.out.println("forname=" + attempt(() -> Class.forName("l5w95seal.B", false, app)));
        System.out.println("find-B-2=" + attempt(() -> find.invoke(app, "l5w95seal.B")));
        System.out.println("load=" + attempt(() -> app.loadClass("l5w95seal.B")));
        System.out.println("find-B-3=" + attempt(() -> find.invoke(app, "l5w95seal.B")));
        System.out.println("find-C=" + attempt(() -> find.invoke(app, "l5w95seal.C")));
    }
}
