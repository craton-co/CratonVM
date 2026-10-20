// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 72, lane L5 -- where a class loaded from a jar
// through a `URLClassLoader` spends its time
// (`docs/internal/proposals/interpreter/i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot-20261104.md`,
// "Progress (wave 72)"). `L5W71JarLoaderClassPathDirectBench` measures the
// whole `loadClass` (~250 us per class on CratonVM after warm-up, ~33 us on
// HotSpot); this bench splits it into steps that each run on their own, so
// `full = delegation miss + resource lookup + bytes + define (+ supertypes)`
// can be read off one run:
//
//   jar-load-plain     fresh `URLClassLoader` (parent: platform) over JARS
//                      jars per round, `loadClass` every class: the whole
//                      path, classes `extends Object` (W71's shape).
//   jar-load-linked    the same over jars whose classes extend the jar's own
//                      `r<j>.Base` and implement `java.io.Serializable` and
//                      `java.lang.Cloneable`: the define's supertype pre-pass
//                      then calls the loader's `loadClass` (W71 never did).
//   mem-define-plain   the define alone: a `URLClassLoader` subclass with the
//                      class bytes in memory calls the protected
//                      `defineClass(name, b, 0, len, CodeSource)` (the
//                      `CodeSource` URLClassLoader would build) for every
//                      class -- no delegation, no lookup, no bytes read.
//   mem-define-linked  the same for the linked family (each jar's `Base`
//                      defined through the pre-pass's `loadClass`).
//   mem-load-plain     that loader's `loadClass`: the platform delegation
//                      miss plus the define, no jar.
//   jar-find-resource  `findResource(path)` of every class over the jars: the
//                      `URLClassPath` search (O(jar index) `JarLoader` probes
//                      per class), no bytes, no define.
//   jar-read-bytes     `getResourceAsStream(path).readAllBytes()`: the search
//                      plus the inflate.
//   platform-miss      `ClassLoader.getPlatformClassLoader().loadClass(name)`
//                      throwing `ClassNotFoundException`: the parent miss every
//                      `URLClassLoader.loadClass` pays (one throwable each).
//
// Every row builds a FRESH loader per round (so every round defines every
// class again) and reports `<row> first ns/class=<n>` (round 0) and
// `<row> median ns/class=<n>` (median of rounds 1..ROUNDS-1). The jars are
// written by the bench itself into a temporary directory (deterministic
// bytes; nothing on `-cp`), so it runs in one step.
//
// Run (interleave the rows' environments, take medians; $B the cratonvm
// binary, $J the JDK 25 bin directory):
//   $J/javac -d out L5W72DefinePathBench.java
//   $B --java-home $J/.. -cp out L5W72DefinePathBench                                      # default
//   env CRATONVM_DEFINE_ONE_PARSE=1 $B --java-home $J/.. -cp out L5W72DefinePathBench      # one parse
//   env CRATONVM_DEFINE_SUPERTYPE_DICTIONARY=1 $B --java-home $J/.. -cp out L5W72DefinePathBench
//   env CRATONVM_DEFINE_ONE_PARSE=1 CRATONVM_DEFINE_SUPERTYPE_DICTIONARY=1 $B --java-home $J/.. -cp out L5W72DefinePathBench
//   $J/java -cp out L5W72DefinePathBench                                                     # HotSpot
// (each also with `--compatible`). One row only: `L5W72DefinePathBench <row>`.
// What the switches should move: CRATONVM_DEFINE_ONE_PARSE the `mem-define-*`
// and `jar-load-*` rows by one class-file parse per class;
// CRATONVM_DEFINE_SUPERTYPE_DICTIONARY the `*-linked` rows by three
// `loadClass` calls per class (Base, Serializable, Cloneable) down to three
// per loader; nothing else.
//
// stdout lines without "ns/" are deterministic (the same on every VM):
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// classes-per-round=800
// jar-load-plain loaded=800
// jar-load-linked loaded=800
// jar-load-linked serializable=800
// mem-define-plain loaded=800
// mem-define-linked loaded=800
// mem-define-linked serializable=800
// mem-load-plain loaded=800
// jar-find-resource found=800
// jar-read-bytes read=800
// jar-read-bytes bytes=3248940
// platform-miss missed=800
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.io.Serializable;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.CodeSigner;
import java.security.CodeSource;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Stream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W72DefinePathBench {
    static final int JARS = 16;
    static final int CLASSES = 50;
    static final int ROUNDS = 5;
    static final int PAD = 4000;
    static final long ENTRY_TIME = 1577836800000L;

    /** `public class <name> extends <superName> implements <interfaces> {}` plus an unused, compressible `CONSTANT_Utf8`. */
    static byte[] classBytes(String name, String superName, String[] interfaces, int pad) throws IOException {
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < pad; i++) {
            text.append((char) ('a' + (i * 7 + name.length()) % 26));
        }
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        // 1 Utf8 name, 2 Class #1, 3 Utf8 super, 4 Class #3, 5 Utf8 pad,
        // then two entries (Utf8, Class) per interface.
        out.writeShort(6 + 2 * interfaces.length);
        out.writeByte(1); out.writeUTF(name);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF(superName);
        out.writeByte(7); out.writeShort(3);
        out.writeByte(1); out.writeUTF(text.toString());
        for (int k = 0; k < interfaces.length; k++) {
            out.writeByte(1); out.writeUTF(interfaces[k]);
            out.writeByte(7); out.writeShort(6 + 2 * k);
        }
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(interfaces.length);
        for (int k = 0; k < interfaces.length; k++) {
            out.writeShort(7 + 2 * k);
        }
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    static final String[] NONE = new String[0];
    static final String[] LINKED_INTERFACES = {"java/io/Serializable", "java/lang/Cloneable"};

    static String plainName(int jar, int i) {
        return "q" + jar + ".P" + i;
    }

    static String linkedName(int jar, int i) {
        return "r" + jar + ".L" + i;
    }

    static String baseName(int jar) {
        return "r" + jar + ".Base";
    }

    /** One family's jars and, per class, its bytes and the `CodeSource` URLClassLoader would build. */
    static final class Family {
        final URL[] urls = new URL[JARS];
        final List<String> names = new ArrayList<>();
        final Map<String, byte[]> bytes = new HashMap<>();
        final Map<String, CodeSource> sources = new HashMap<>();
    }

    static void putEntry(ZipOutputStream zip, String entryName, byte[] content) throws IOException {
        ZipEntry entry = new ZipEntry(entryName);
        entry.setTime(ENTRY_TIME);
        zip.putNextEntry(entry);
        zip.write(content);
        zip.closeEntry();
    }

    static Family make(Path dir, boolean linked) throws IOException {
        Family family = new Family();
        for (int j = 0; j < JARS; j++) {
            Path jar = dir.resolve((linked ? "l5w72r" : "l5w72q") + j + ".jar");
            URL url = jar.toUri().toURL();
            family.urls[j] = url;
            CodeSource source = new CodeSource(url, (CodeSigner[]) null);
            try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(jar))) {
                if (linked) {
                    String base = baseName(j).replace('.', '/');
                    byte[] b = classBytes(base, "java/lang/Object", NONE, 16);
                    putEntry(zip, base + ".class", b);
                    family.bytes.put(baseName(j), b);
                    family.sources.put(baseName(j), source);
                }
                for (int i = 0; i < CLASSES; i++) {
                    String name = linked ? linkedName(j, i) : plainName(j, i);
                    String internal = name.replace('.', '/');
                    byte[] b = linked
                            ? classBytes(internal, baseName(j).replace('.', '/'), LINKED_INTERFACES, PAD)
                            : classBytes(internal, "java/lang/Object", NONE, PAD);
                    putEntry(zip, internal + ".class", b);
                    family.names.add(name);
                    family.bytes.put(name, b);
                    family.sources.put(name, source);
                }
            }
        }
        return family;
    }

    /** A `URLClassLoader` (parent: platform) holding the classes' bytes in memory. */
    static final class MemLoader extends URLClassLoader {
        final Family family;

        MemLoader(Family family) {
            super(new URL[0], ClassLoader.getPlatformClassLoader());
            this.family = family;
        }

        Class<?> defineDirect(String name) {
            byte[] b = family.bytes.get(name);
            return defineClass(name, b, 0, b.length, family.sources.get(name));
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = family.bytes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length, family.sources.get(name));
        }
    }

    /** One round of a row: the count it reports, and the work it times. */
    interface Round {
        long run() throws Exception;
    }

    interface RoundFactory {
        Round make();
    }

    static void row(String label, String countLabel, RoundFactory factory, String extra) throws Exception {
        long[] perClass = new long[ROUNDS];
        long count = 0;
        int classes = JARS * CLASSES;
        for (int r = 0; r < ROUNDS; r++) {
            Round round = factory.make();
            long t0 = System.nanoTime();
            count = round.run();
            perClass[r] = (System.nanoTime() - t0) / classes;
        }
        long[] later = Arrays.copyOfRange(perClass, 1, ROUNDS);
        Arrays.sort(later);
        System.out.println(label + " " + countLabel + "=" + count + (extra == null ? "" : " " + extra));
        System.out.println(label + " first ns/class=" + perClass[0]);
        System.out.println(label + " median ns/class=" + later[later.length / 2]);
    }

    static int serializableSeen;

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        Path dir = Files.createTempDirectory("l5w72");
        try {
            Family plain = make(dir, false);
            Family linked = make(dir, true);
            System.out.println("classes-per-round=" + (JARS * CLASSES));
            ClassLoader platform = ClassLoader.getPlatformClassLoader();

            if (only == null || only.equals("jar-load-plain")) {
                row("jar-load-plain", "loaded", () -> () -> {
                    long n = 0;
                    try (URLClassLoader ucl = new URLClassLoader(plain.urls, platform)) {
                        for (String name : plain.names) {
                            if (ucl.loadClass(name).getClassLoader() == ucl) {
                                n++;
                            }
                        }
                    }
                    return n;
                }, null);
            }
            if (only == null || only.equals("jar-load-linked")) {
                serializableSeen = 0;
                row("jar-load-linked", "loaded", () -> () -> {
                    long n = 0;
                    int ser = 0;
                    try (URLClassLoader ucl = new URLClassLoader(linked.urls, platform)) {
                        for (String name : linked.names) {
                            Class<?> c = ucl.loadClass(name);
                            if (c.getClassLoader() == ucl) {
                                n++;
                            }
                            if (Serializable.class.isAssignableFrom(c)) {
                                ser++;
                            }
                        }
                    }
                    serializableSeen = ser;
                    return n;
                }, null);
                System.out.println("jar-load-linked serializable=" + serializableSeen);
            }
            if (only == null || only.equals("mem-define-plain")) {
                row("mem-define-plain", "loaded", () -> () -> {
                    long n = 0;
                    try (MemLoader loader = new MemLoader(plain)) {
                        for (String name : plain.names) {
                            if (loader.defineDirect(name).getClassLoader() == loader) {
                                n++;
                            }
                        }
                    }
                    return n;
                }, null);
            }
            if (only == null || only.equals("mem-define-linked")) {
                serializableSeen = 0;
                row("mem-define-linked", "loaded", () -> () -> {
                    long n = 0;
                    int ser = 0;
                    try (MemLoader loader = new MemLoader(linked)) {
                        for (String name : linked.names) {
                            Class<?> c = loader.defineDirect(name);
                            if (c.getClassLoader() == loader) {
                                n++;
                            }
                            if (Serializable.class.isAssignableFrom(c)) {
                                ser++;
                            }
                        }
                    }
                    serializableSeen = ser;
                    return n;
                }, null);
                System.out.println("mem-define-linked serializable=" + serializableSeen);
            }
            if (only == null || only.equals("mem-load-plain")) {
                row("mem-load-plain", "loaded", () -> () -> {
                    long n = 0;
                    try (MemLoader loader = new MemLoader(plain)) {
                        for (String name : plain.names) {
                            if (loader.loadClass(name).getClassLoader() == loader) {
                                n++;
                            }
                        }
                    }
                    return n;
                }, null);
            }
            if (only == null || only.equals("jar-find-resource")) {
                row("jar-find-resource", "found", () -> () -> {
                    long n = 0;
                    try (URLClassLoader ucl = new URLClassLoader(plain.urls, platform)) {
                        for (String name : plain.names) {
                            if (ucl.findResource(name.replace('.', '/') + ".class") != null) {
                                n++;
                            }
                        }
                    }
                    return n;
                }, null);
            }
            if (only == null || only.equals("jar-read-bytes")) {
                long[] total = new long[1];
                row("jar-read-bytes", "read", () -> () -> {
                    long n = 0;
                    long size = 0;
                    try (URLClassLoader ucl = new URLClassLoader(plain.urls, platform)) {
                        for (String name : plain.names) {
                            try (InputStream in = ucl.getResourceAsStream(name.replace('.', '/') + ".class")) {
                                if (in != null) {
                                    size += in.readAllBytes().length;
                                    n++;
                                }
                            }
                        }
                    }
                    total[0] = size;
                    return n;
                }, null);
                System.out.println("jar-read-bytes bytes=" + total[0]);
            }
            if (only == null || only.equals("platform-miss")) {
                row("platform-miss", "missed", () -> () -> {
                    long n = 0;
                    for (String name : plain.names) {
                        try {
                            platform.loadClass(name);
                        } catch (ClassNotFoundException expected) {
                            n++;
                        }
                    }
                    return n;
                }, null);
            }
        } finally {
            try (Stream<Path> walk = Files.walk(dir)) {
                walk.sorted(Comparator.reverseOrder()).map(Path::toFile).forEach(File::delete);
            }
        }
    }
}
