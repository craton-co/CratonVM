// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 70, lane L5 -- what
// `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE` (default off) saves: the
// jar-loader bytes row (`CRATONVM_JAR_LOADER_BYTES_INDEX`) of a jar that is
// ON THE VM'S APPLICATION CLASS PATH built from the class path's own mapping
// and parse instead of a second `open` + `mmap` + central-directory parse of
// the same file
// (`docs/internal/fixed-bugs/interpreter-i69-L5-proposal-lend-the-class-path-archive-to-the-jar-loader-bytes-index-KEPT-OFF-20261104.md`).
// This is the `--jdk-only` shape of every `-cp` jar: the JDK's
// `AppClassLoader` reads it through `URLClassPath$JarLoader`; here a
// `URLClassLoader` over the same `file:` URLs (parent: the platform loader)
// stands in for it so that every round defines every class afresh.
//
// The jars must be on `-cp` when the VM starts, so: `--make <dir>` writes
// JARS jars `<dir>/l5w70j<k>.jar` of CLASSES deflated ~4 KB classes each
// (any JVM, deterministic bytes) and prints the class-path suffix to use;
// then the bench runs with them on the class path. Round 0 builds one row per
// jar: its `ns/class` is where the switch shows (no map, no parse); later
// rounds only read entries (the same work either way).
//
// Run (interleave, take medians; Linux):
//   javac -d out L5W70JarLoaderClassPathArchiveBench.java
//   CP=$(java -cp out L5W70JarLoaderClassPathArchiveBench --make /tmp/l5w70b | tail -1)
//   B="CRATONVM_JAR_LOADER_BYTES_INDEX=1"
//   env $B CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE=1 cratonvm --java-home <jdk25> -cp out:$CP L5W70JarLoaderClassPathArchiveBench
//   env $B cratonvm --java-home <jdk25> -cp out:$CP L5W70JarLoaderClassPathArchiveBench     # control: a second map + parse per jar
//   java -cp out:$CP L5W70JarLoaderClassPathArchiveBench                                    # HotSpot
// System calls: `strace -f -c -e trace=openat,mmap,munmap,newfstatat`: with
// the switch one `openat` and one `mmap` fewer per jar (JARS of each; one
// `stat` per row build remains, the stamp check).
//
// Prints to stderr: `round=<k> ns/class=<n>`, `first-round ns/class=<n>`,
// `later-rounds median ns/class=<n>`, and from `/proc/self/maps` after round
// 0 `jar-mappings=<n> jar-mapped-kib=<n>` (mappings whose path is one of the
// bench's jars: JARS with the switch -- the class path's own --, 2 x JARS
// without it, 0 on HotSpot, whose `ZipFile` reads instead of mapping) and
// `VmRSS-kib=<n>` (`maps=unavailable` off Linux). stdout is
// `on-class-path=true` and `loaded=<n> per round`, the same on every VM.
// Positive control: `CRATONVM_DBG=access` LOADER census
// `jar-loader-class-path-lent=<JARS>` with the switch, `0` without, and
// `jar-loader-bytes-served=<JARS * CLASSES * ROUNDS>` either way.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// on-class-path=true
// loaded=1600 per round
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W70JarLoaderClassPathArchiveBench {
    static final int JARS = 32;
    static final int CLASSES = 50;
    static final int ROUNDS = 7;
    static final long ENTRY_TIME = 1577836800000L;

    /** `public class <name> extends Object {}` padded with an unused, compressible `CONSTANT_Utf8`. */
    static byte[] paddedClass(String name, int pad) throws IOException {
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < pad; i++) {
            text.append((char) ('a' + (i * 7 + name.length()) % 26));
        }
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(6);
        out.writeByte(1); out.writeUTF(name);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF("java/lang/Object");
        out.writeByte(7); out.writeShort(3);
        out.writeByte(1); out.writeUTF(text.toString());
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    static String className(int jar, int i) {
        return "p" + jar + ".C" + i;
    }

    static void make(Path dir) throws IOException {
        Files.createDirectories(dir);
        StringBuilder cp = new StringBuilder();
        for (int j = 0; j < JARS; j++) {
            Path jar = dir.resolve("l5w70j" + j + ".jar").toAbsolutePath();
            try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(jar))) {
                ZipEntry pkg = new ZipEntry("p" + j + "/");
                pkg.setTime(ENTRY_TIME);
                zip.putNextEntry(pkg);
                zip.closeEntry();
                for (int i = 0; i < CLASSES; i++) {
                    String internal = className(j, i).replace('.', '/');
                    ZipEntry entry = new ZipEntry(internal + ".class");
                    entry.setTime(ENTRY_TIME);
                    zip.putNextEntry(entry);
                    zip.write(paddedClass(internal, 4000));
                    zip.closeEntry();
                }
            }
            if (j > 0) {
                cp.append(File.pathSeparator);
            }
            cp.append(jar);
        }
        System.out.println(cp);
    }

    /** The bench's jars on the class path (absolute), in class-path order. */
    static List<Path> jarsOnClassPath() {
        List<Path> jars = new ArrayList<>();
        for (String element : System.getProperty("java.class.path").split(File.pathSeparator)) {
            String file = Paths.get(element).getFileName() == null ? "" : Paths.get(element).getFileName().toString();
            if (file.startsWith("l5w70j") && file.endsWith(".jar")) {
                jars.add(Paths.get(element).toAbsolutePath());
            }
        }
        return jars;
    }

    /** `jar-mappings=<n> jar-mapped-kib=<n> VmRSS-kib=<n>` from /proc/self, or `maps=unavailable`. */
    static String mappings(List<Path> jars) {
        Path maps = Paths.get("/proc/self/maps");
        if (!Files.isReadable(maps)) {
            return "maps=unavailable";
        }
        try {
            Set<String> names = new HashSet<>();
            for (Path jar : jars) {
                names.add(jar.toString());
                names.add(jar.toRealPath().toString());
            }
            int count = 0;
            long bytes = 0;
            for (String line : Files.readAllLines(maps)) {
                int slash = line.indexOf('/');
                if (slash < 0 || !names.contains(line.substring(slash).trim())) {
                    continue;
                }
                String[] range = line.substring(0, line.indexOf(' ')).split("-");
                count++;
                bytes += Long.parseUnsignedLong(range[1], 16) - Long.parseUnsignedLong(range[0], 16);
            }
            String rss = "?";
            for (String line : Files.readAllLines(Paths.get("/proc/self/status"))) {
                if (line.startsWith("VmRSS:")) {
                    rss = line.substring("VmRSS:".length()).trim().split("\\s+")[0];
                }
            }
            return "jar-mappings=" + count + " jar-mapped-kib=" + (bytes / 1024) + " VmRSS-kib=" + rss;
        } catch (IOException | RuntimeException e) {
            return "maps=" + e.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("--make")) {
            make(Paths.get(args[1]));
            return;
        }
        List<Path> jars = jarsOnClassPath();
        System.out.println("on-class-path=" + (jars.size() == JARS));
        if (jars.size() != JARS) {
            return;
        }
        URL[] urls = new URL[JARS];
        for (int j = 0; j < JARS; j++) {
            urls[j] = jars.get(j).toUri().toURL();
        }
        long[] perClass = new long[ROUNDS];
        int loaded = 0;
        for (int r = 0; r < ROUNDS; r++) {
            loaded = 0;
            try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
                long t0 = System.nanoTime();
                for (int j = 0; j < JARS; j++) {
                    for (int i = 0; i < CLASSES; i++) {
                        if (ucl.loadClass(className(j, i)).getClassLoader() == ucl) {
                            loaded++;
                        }
                    }
                }
                perClass[r] = (System.nanoTime() - t0) / ((long) JARS * CLASSES);
                if (r == 0) {
                    // While the loader (and the JDK's JarFiles) are still open.
                    System.err.println(mappings(jars));
                }
            }
            System.err.println("round=" + r + " ns/class=" + perClass[r]);
        }
        long[] later = Arrays.copyOfRange(perClass, 1, ROUNDS);
        Arrays.sort(later);
        System.err.println("first-round ns/class=" + perClass[0]);
        System.err.println("later-rounds median ns/class=" + later[later.length / 2]);
        System.out.println("loaded=" + loaded + " per round");
    }
}
