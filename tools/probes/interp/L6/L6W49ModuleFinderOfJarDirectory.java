// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L6: `ModuleFinder.of(<directory>)` finds
// the modular and automatic jars in the directory under `--compatible`, as on
// HotSpot and under `--jdk-only`
// (`docs/internal/fixed-bugs/interpreter-L6-compatible-module-finder-of-a-jar-directory-finds-no-module-FIXED-20261013.md`).
// Rows, each a step of `ModulePath.readJar` done by hand first (the `jar-*`
// rows), then through the finder. The `finder` row runs first and again
// last.
//
// HotSpot 25.0.3 prints (measured; the same with -Xint):
//     finder=[auto.l6w49, m49j]
//     jar-entry=module-info.class 1
//     jar-versioned=[META-INF/MANIFEST.MF, auto/l6w49/A.class]
//     jar-manifest=null
//     descriptor=m49j [p49j] exports=[p49j]
//     automatic=auto.l6w49 true [auto.l6w49]
//     find=true true
//     finder-again=[auto.l6w49, m49j]
//
//     javac -d out L6W49ModuleFinderOfJarDirectory.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W49ModuleFinderOfJarDirectory

import java.io.*;
import java.lang.module.*;
import java.nio.file.*;
import java.util.*;
import java.util.jar.*;
import java.util.stream.Collectors;
import java.util.zip.ZipEntry;

public class L6W49ModuleFinderOfJarDirectory {
    static final String P = "L6W49ModuleFinderOfJarDirectory$";

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    static byte[] own(String n) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(n + ".class")) { return in.readAllBytes(); }
    }

    /** A minimal module-info.class for `module m49j { exports p49j; }`. */
    static byte[] moduleInfo() throws IOException {
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buf);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(69);
        // constant pool: 1 Class(2) 2 Utf8 module-info 3 Utf8 Module 4 Module(5) 5 Utf8 m49j
        // 6 Package(7) 7 Utf8 p49j 8 Module(9) 9 Utf8 java.base 10 Utf8 ModulePackages
        out.writeShort(11);
        out.writeByte(7); out.writeShort(2);
        out.writeByte(1); out.writeUTF("module-info");
        out.writeByte(1); out.writeUTF("Module");
        out.writeByte(19); out.writeShort(5);
        out.writeByte(1); out.writeUTF("m49j");
        out.writeByte(20); out.writeShort(7);
        out.writeByte(1); out.writeUTF("p49j");
        out.writeByte(19); out.writeShort(9);
        out.writeByte(1); out.writeUTF("java.base");
        out.writeByte(1); out.writeUTF("ModulePackages");
        out.writeShort(0x8000); // ACC_MODULE
        out.writeShort(1);
        out.writeShort(0);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(0); // methods
        out.writeShort(2); // attributes
        // Module attribute
        ByteArrayOutputStream mb = new ByteArrayOutputStream();
        DataOutputStream m = new DataOutputStream(mb);
        m.writeShort(4); m.writeShort(0); m.writeShort(0); // name, flags, version
        m.writeShort(1); m.writeShort(8); m.writeShort(0x8000); m.writeShort(0); // requires java.base mandated
        m.writeShort(1); m.writeShort(6); m.writeShort(0); m.writeShort(0); // exports p49j
        m.writeShort(0); m.writeShort(0); m.writeShort(0); // opens, uses, provides
        out.writeShort(3); out.writeInt(mb.size()); out.write(mb.toByteArray());
        // ModulePackages attribute
        out.writeShort(10); out.writeInt(4); out.writeShort(1); out.writeShort(6);
        return buf.toByteArray();
    }

    static void jar(Path file, Map<String, byte[]> entries, Manifest mf) throws IOException {
        try (OutputStream fo = Files.newOutputStream(file);
             JarOutputStream jo = mf == null ? new JarOutputStream(fo) : new JarOutputStream(fo, mf)) {
            for (Map.Entry<String, byte[]> e : entries.entrySet()) {
                jo.putNextEntry(new ZipEntry(e.getKey()));
                jo.write(e.getValue());
                jo.closeEntry();
            }
        }
    }

    static void deleteTree(Path dir) throws IOException {
        try (var s = Files.walk(dir)) {
            for (Path p : s.sorted(Comparator.reverseOrder()).toList()) Files.deleteIfExists(p);
        }
    }

    static String names(ModuleFinder f) {
        return f.findAll().stream().map(r -> r.descriptor().name()).sorted().toList().toString();
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l6w49mods");
        try {
            Path modular = dir.resolve("m49j.jar");
            Map<String, byte[]> m = new TreeMap<>();
            m.put("module-info.class", moduleInfo());
            m.put("p49j/J.class", own(P + "J"));
            jar(modular, m, null);
            Map<String, byte[]> a = new TreeMap<>();
            a.put("auto/l6w49/A.class", own(P + "J"));
            jar(dir.resolve("auto.l6w49.jar"), a, new Manifest());

            row("finder", () -> names(ModuleFinder.of(dir)));
            row("jar-entry", () -> {
                try (JarFile jf = new JarFile(modular.toFile(), true, ZipFile.OPEN_READ, JarFile.runtimeVersion())) {
                    JarEntry e = jf.getJarEntry("module-info.class");
                    return e.getName() + " " + (e.getSize() > 0 ? 1 : 0);
                }
            });
            row("jar-versioned", () -> {
                try (JarFile jf = new JarFile(dir.resolve("auto.l6w49.jar").toFile(), true, ZipFile.OPEN_READ,
                        JarFile.runtimeVersion())) {
                    return jf.versionedStream().map(JarEntry::getName).sorted().toList();
                }
            });
            row("jar-manifest", () -> {
                try (JarFile jf = new JarFile(modular.toFile(), true, ZipFile.OPEN_READ, JarFile.runtimeVersion())) {
                    return jf.getManifest();
                }
            });
            row("descriptor", () -> {
                try (JarFile jf = new JarFile(modular.toFile())) {
                    ModuleDescriptor d = ModuleDescriptor.read(jf.getInputStream(jf.getJarEntry("module-info.class")));
                    return d.name() + " " + new TreeSet<>(d.packages()) + " exports="
                            + d.exports().stream().map(ModuleDescriptor.Exports::source).sorted().toList();
                }
            });
            row("automatic", () -> {
                ModuleReference r = ModuleFinder.of(dir.resolve("auto.l6w49.jar")).findAll().iterator().next();
                return r.descriptor().name() + " " + r.descriptor().isAutomatic() + " "
                        + new TreeSet<>(r.descriptor().packages());
            });
            row("find", () -> ModuleFinder.of(dir).find("m49j").isPresent() + " "
                    + ModuleFinder.of(dir).find("auto.l6w49").isPresent());
            row("finder-again", () -> names(ModuleFinder.of(dir)));
        } finally {
            deleteTree(dir);
        }
    }

    static final class ZipFile {
        static final int OPEN_READ = java.util.zip.ZipFile.OPEN_READ;
    }

    public static class J {
    }
}
