// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 72, lane L5 -- how often the VM asks a defining
// loader's `loadClass` for the supertypes of the classes it defines
// (`CRATONVM_DEFINE_SUPERTYPE_DICTIONARY`, default off;
// `docs/internal/proposals/interpreter/i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot-20261104.md`,
// "Progress (wave 72)").
//
// HotSpot resolves a class's superclass and interfaces at define time
// through `SystemDictionary::resolve_super_or_fail`, which looks in the
// defining loader's dictionary first and calls the loader's `loadClass` only
// on a miss; the dictionary holds the classes the loader defined and, after
// such a call returns another loader's class, that class too
// (`load_instance_class` -> `update_dictionary`). So for a loader that
// defines three classes `p.Ci extends p.Base implements Serializable,
// Cloneable`, `loadClass` is asked once for each of the three supertype names
// (the first define), never again. CratonVM's define natives asked it for
// every supertype other than `java.lang.Object` on every define (three times
// each here) until the switch.
//
// The loader counts the names its `loadClass(String, boolean)` is asked for
// (a `URLClassLoader` over one jar, parent the platform loader); the probe
// itself asks only for the three `p.Ci`.
//
// Run ($B the cratonvm binary, $J the JDK 25 bin directory):
//   $J/javac -d out L5W72SupertypeDictionary.java
//   env CRATONVM_DEFINE_SUPERTYPE_DICTIONARY=1 $B --java-home $J/.. [--compatible] -cp out L5W72SupertypeDictionary
//   $B --java-home $J/.. [--compatible] -cp out L5W72SupertypeDictionary    # without: asks=3 for each name
//   $J/java -cp out L5W72SupertypeDictionary                                # HotSpot
//
// The expected block below is HotSpot's dictionary rule read from its
// sources (`systemDictionary.cpp`, JDK 25); confirm it on the host.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// defined=3
// asks p.Base=1
// asks java.io.Serializable=1
// asks java.lang.Cloneable=1
// asks p.C0=1
// serializable=true cloneable=true super=p.Base
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.Serializable;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import java.util.Map;
import java.util.TreeMap;
import java.util.stream.Stream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W72SupertypeDictionary {
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

    static void putEntry(ZipOutputStream zip, String entryName, byte[] content) throws IOException {
        ZipEntry entry = new ZipEntry(entryName);
        entry.setTime(1577836800000L);
        zip.putNextEntry(entry);
        zip.write(content);
        zip.closeEntry();
    }

    /** Counts every name its `loadClass` is asked for. */
    static final class CountingLoader extends URLClassLoader {
        final Map<String, Integer> asks = new TreeMap<>();

        CountingLoader(URL[] urls) {
            super(urls, ClassLoader.getPlatformClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (asks) {
                asks.merge(name, 1, Integer::sum);
            }
            return super.loadClass(name, resolve);
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w72s");
        try {
            Path jar = dir.resolve("l5w72s.jar");
            try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(jar))) {
                putEntry(zip, "p/Base.class", classBytes("p/Base", "java/lang/Object"));
                for (int i = 0; i < 3; i++) {
                    putEntry(zip, "p/C" + i + ".class",
                            classBytes("p/C" + i, "p/Base", "java/io/Serializable", "java/lang/Cloneable"));
                }
            }
            try (CountingLoader loader = new CountingLoader(new URL[] {jar.toUri().toURL()})) {
                int defined = 0;
                Class<?> last = null;
                for (int i = 0; i < 3; i++) {
                    last = loader.loadClass("p.C" + i);
                    if (last.getClassLoader() == loader) {
                        defined++;
                    }
                }
                System.out.println("defined=" + defined);
                for (String name : new String[] {"p.Base", "java.io.Serializable", "java.lang.Cloneable", "p.C0"}) {
                    Integer n;
                    synchronized (loader.asks) {
                        n = loader.asks.get(name);
                    }
                    System.out.println("asks " + name + "=" + (n == null ? 0 : n));
                }
                System.out.println("serializable=" + Serializable.class.isAssignableFrom(last)
                        + " cloneable=" + Cloneable.class.isAssignableFrom(last)
                        + " super=" + last.getSuperclass().getName());
            }
        } finally {
            try (Stream<Path> walk = Files.walk(dir)) {
                walk.sorted(Comparator.reverseOrder()).map(Path::toFile).forEach(File::delete);
            }
        }
    }
}
