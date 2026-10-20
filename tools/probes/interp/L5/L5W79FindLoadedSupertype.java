// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 79, lane L5 -- `findLoadedClass` on a supertype
// the loader resolved for its own define
// (`docs/internal/fixed-bugs/interpreter-i72-L5-findloadedclass-misses-a-jdk-supertype-the-loader-resolved-FIXED-20261111.md`).
//
// HotSpot resolves a class's superclass and interfaces at define time through
// `SystemDictionary::resolve_super_or_fail` with the DEFINING loader; when the
// loader's `loadClass` answers a class some other loader defined,
// `load_instance_class` files it in the defining loader's dictionary
// (`update_dictionary`), and `findLoadedClass` (`JVM_FindLoadedClass` ->
// `find_instance_klass`) reads that dictionary. So after a user loader `L`
// (parent: a user loader `P`) defines `p.C extends p.Base implements
// java.io.Serializable`, with `p.Base` defined by `P`:
//   L.findLoadedClass("p.Base") and ("java.io.Serializable") answer the class;
//   L.findLoadedClass("java.lang.Cloneable") -- never resolved -- is null;
//   P.findLoadedClass("java.io.Serializable") is null (P never resolved it).
// CratonVM answered null for both supertypes. With
// CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION=1 the define pre-pass records
// `L` as an initiating loader of each supertype it resolves (the initiating
// records exist under --jdk-only only; --compatible keeps answering null
// unless CRATONVM_COMPAT_INITIATING_RECORDS=1 too).
//
// Run ($B the cratonvm binary, $J the JDK 25 bin directory):
//   $J/javac -d out L5W79FindLoadedSupertype.java
//   $J/java -cp out L5W79FindLoadedSupertype                                   # HotSpot
//   env CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION=1 $B --java-home $J/.. -cp out L5W79FindLoadedSupertype
//   $B --java-home $J/.. -cp out L5W79FindLoadedSupertype      # default: the two `L super` lines say false
//   env CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION=1 CRATONVM_COMPAT_INITIATING_RECORDS=1 $B --compatible --java-home $J/.. -cp out L5W79FindLoadedSupertype
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// c-loader=L base-loader=P
// L own p.C=true
// L super p.Base=true
// L super java.io.Serializable=true
// L never java.lang.Cloneable=false
// P own p.Base=true
// P never java.io.Serializable=false
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import java.util.stream.Stream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W79FindLoadedSupertype {
    /** `public class <name> extends <superName> implements <interfaces> {}` (no members). */
    static byte[] classBytes(String name, String superName, String... interfaces) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(5 + 2 * interfaces.length);
        out.writeByte(1); out.writeUTF(name);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF(superName);
        out.writeByte(7); out.writeShort(3);
        for (int k = 0; k < interfaces.length; k++) {
            out.writeByte(1); out.writeUTF(interfaces[k]);
            out.writeByte(7); out.writeShort(5 + 2 * k);
        }
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(interfaces.length);
        for (int k = 0; k < interfaces.length; k++) {
            out.writeShort(6 + 2 * k);
        }
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    static void writeJar(Path jar, String entryName, byte[] content) throws IOException {
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(jar))) {
            ZipEntry entry = new ZipEntry(entryName);
            entry.setTime(1577836800000L);
            zip.putNextEntry(entry);
            zip.write(content);
            zip.closeEntry();
        }
    }

    /** A `URLClassLoader` that exposes the protected `findLoadedClass`. */
    static final class Exposing extends URLClassLoader {
        Exposing(URL[] urls, ClassLoader parent) {
            super(urls, parent);
        }

        boolean has(String name) {
            return findLoadedClass(name) != null;
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w79f");
        try {
            Path baseJar = dir.resolve("base.jar");
            Path cJar = dir.resolve("c.jar");
            writeJar(baseJar, "p/Base.class", classBytes("p/Base", "java/lang/Object"));
            writeJar(cJar, "p/C.class", classBytes("p/C", "p/Base", "java/io/Serializable"));
            try (Exposing parent = new Exposing(new URL[] {baseJar.toUri().toURL()},
                            ClassLoader.getPlatformClassLoader());
                    Exposing loader = new Exposing(new URL[] {cJar.toUri().toURL()}, parent)) {
                Class<?> c = loader.loadClass("p.C");
                Class<?> base = c.getSuperclass();
                System.out.println("c-loader=" + (c.getClassLoader() == loader ? "L" : "other")
                        + " base-loader=" + (base.getClassLoader() == parent ? "P" : "other"));
                System.out.println("L own p.C=" + loader.has("p.C"));
                System.out.println("L super p.Base=" + loader.has("p.Base"));
                System.out.println("L super java.io.Serializable=" + loader.has("java.io.Serializable"));
                System.out.println("L never java.lang.Cloneable=" + loader.has("java.lang.Cloneable"));
                System.out.println("P own p.Base=" + parent.has("p.Base"));
                System.out.println("P never java.io.Serializable=" + parent.has("java.io.Serializable"));
            }
        } finally {
            try (Stream<Path> walk = Files.walk(dir)) {
                walk.sorted(Comparator.reverseOrder()).map(Path::toFile).forEach(File::delete);
            }
        }
    }
}
