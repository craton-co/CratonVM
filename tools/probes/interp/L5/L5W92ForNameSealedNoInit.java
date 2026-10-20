// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L5 -- `Class.forName(n, false, app)` of a
// class whose package is sealed by ANOTHER class-path JAR is the
// `SecurityException` HotSpot raises at the class's define
// (`BuiltinClassLoader.defineClass(cn, Resource)` -> `defineOrCheckPackage` ->
// `getAndVerifyPackage`), not the class. The VM ran the JDK's step at LINK only
// (wave 62), so a load that does not link escaped it
// (`i58-L5-proposal-a-class-path-package-is-defined-when-its-first-class-is`).
//
// The probe builds its own sealed JAR: `setup <dir>` writes `<dir>/a.jar`
// (manifest `Sealed: true`, class `l5w92seal.A`) and `<dir>/b.jar` (class
// `l5w92seal.B`, no manifest). Both classes are minimal class files the probe
// emits itself (public, no members), so no second source file is needed.
//
// Rows (each first, and again last):
//   a-noinit-*   `forName("l5w92seal.A", false, app)`: the package's first
//                class, from the sealing JAR (defines the package sealed)
//   b-noinit-*   `forName("l5w92seal.B", false, app)`: B is in b.jar
//   b-init-*     `forName("l5w92seal.B", true, app)`
//   b-load-*     `app.loadClass("l5w92seal.B")`
//   sealed-*     `app.getDefinedPackage("l5w92seal")`: isSealed, and whether
//                the seal base is a.jar
//
// SETUP and run (one directory; `;` for `:` on Windows):
//   javac -d out L5W92ForNameSealedNoInit.java
//   java -cp out L5W92ForNameSealedNoInit setup .
//   java -cp out:a.jar:b.jar L5W92ForNameSealedNoInit
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out:a.jar:b.jar L5W92ForNameSealedNoInit
//   (`CRATONVM_FORNAME_CHECKS_PACKAGE=0` is the kill switch: `--compatible`
//   then prints `b-noinit-N=l5w92seal.B`.)
//
// HOTSPOT_EXPECTED (derived from the JDK 25 sources: `getAndVerifyPackage`
// throws `new SecurityException("sealing violation: package " + pn + " is
// sealed")` for a sealed package whose seal base is not the class's code
// source, on every define attempt; B is never defined, so every door asks
// again; the same text the wave-62 probe measured on HotSpot 25.0.3):
//   a-noinit-1=l5w92seal.A
//   b-noinit-1=java.lang.SecurityException msg=sealing violation: package l5w92seal is sealed
//   b-init-1=java.lang.SecurityException msg=sealing violation: package l5w92seal is sealed
//   b-load-1=java.lang.SecurityException msg=sealing violation: package l5w92seal is sealed
//   sealed-1=sealed=true base=a.jar
//   a-noinit-2=l5w92seal.A
//   b-noinit-2=java.lang.SecurityException msg=sealing violation: package l5w92seal is sealed
//   b-init-2=java.lang.SecurityException msg=sealing violation: package l5w92seal is sealed
//   b-load-2=java.lang.SecurityException msg=sealing violation: package l5w92seal is sealed
//   sealed-2=sealed=true base=a.jar

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.net.URL;
import java.util.concurrent.Callable;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L5W92ForNameSealedNoInit {
    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
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

    static void jar(File file, Manifest man, String internal) throws IOException {
        try (JarOutputStream out = man == null
                ? new JarOutputStream(new FileOutputStream(file))
                : new JarOutputStream(new FileOutputStream(file), man)) {
            out.putNextEntry(new JarEntry(internal + ".class"));
            out.write(emptyClass(internal));
            out.closeEntry();
        }
    }

    static void setup(File dir) throws IOException {
        Manifest man = new Manifest();
        man.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        man.getMainAttributes().put(Attributes.Name.SEALED, "true");
        jar(new File(dir, "a.jar"), man, "l5w92seal/A");
        jar(new File(dir, "b.jar"), null, "l5w92seal/B");
        System.out.println("wrote " + new File(dir, "a.jar") + " and " + new File(dir, "b.jar"));
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("setup")) {
            setup(new File(args[1]));
            return;
        }
        ClassLoader app = ClassLoader.getSystemClassLoader();
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("a-noinit-" + tag + "=" + attempt(() ->
                    Class.forName("l5w92seal.A", false, app).getName()));
            System.out.println("b-noinit-" + tag + "=" + attempt(() ->
                    Class.forName("l5w92seal.B", false, app).getName()));
            System.out.println("b-init-" + tag + "=" + attempt(() ->
                    Class.forName("l5w92seal.B", true, app).getName()));
            System.out.println("b-load-" + tag + "=" + attempt(() ->
                    app.loadClass("l5w92seal.B").getName()));
            System.out.println("sealed-" + tag + "=" + attempt(() -> {
                Package p = app.getDefinedPackage("l5w92seal");
                if (p == null) {
                    return "null";
                }
                URL base = Class.forName("l5w92seal.A", false, app)
                        .getProtectionDomain().getCodeSource().getLocation();
                String file = base.getPath().substring(base.getPath().lastIndexOf('/') + 1);
                return "sealed=" + p.isSealed() + " base=" + (p.isSealed(base) ? file : "other");
            }));
        }
    }
}
