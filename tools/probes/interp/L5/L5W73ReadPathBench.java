// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 73, lane L5 -- where `L5W72DefinePathBench`'s
// `jar-read-bytes` row (~93 us per class on CratonVM against ~24 us on HotSpot
// 25) spends its time
// (`docs/internal/proposals/interpreter/i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot-20261104.md`,
// "Progress (wave 73)").
//
// `jar-read-bytes` is `URLClassLoader.getResourceAsStream(path).readAllBytes()`,
// which is NOT the path `loadClass` reads a class through: it asks the parent
// (`ClassLoader.getResource` -> the platform loader), searches the jars
// (`findResource`, the `jar-find-resource` row), then opens the `jar:` URL
// (`JarURLConnection`, the `JarFileFactory` cache, a second `ZipFile.getEntry`)
// and reads the entry's `ZipFileInflaterInputStream`. `loadClass` reads through
// `URLClassPath$JarLoader`'s `Resource.getBytes()` (`readNBytes(size)` of
// `JarFile.getInputStream(entry)`), with no URL connection and no parent
// `getResource`. These rows run each leg on its own, over the same 16 jars x 50
// classes (`extends Object`, a 4000-character `Utf8`, ~4 KB each):
//
//   ucl-resource-stream  fresh `URLClassLoader` (parent: platform) per round,
//                        `getResourceAsStream(path).readAllBytes()`: the
//                        whole of W72's `jar-read-bytes`, as the reference.
//   parent-get-resource  `platformLoader.getResource(path)` (null for every
//                        class): the parent leg of `getResource`
//                        (CRATONVM_PLATFORM_RESOURCE_JRT_WALK moves this).
//   ucl-find-resource    fresh `URLClassLoader`, `findResource(path)`: the
//                        `URLClassPath` search (W72's `jar-find-resource`).
//   url-open-stream      the class URLs `findResource` answered (found once,
//                        outside the timing), `url.openStream().readAllBytes()`
//                        per round: the `jar:` connection plus the read.
//   jar-get-entry        16 `JarFile`s opened outside the timing, each class's
//                        `getJarEntry(path)` on its own jar: one CEN hit.
//   jar-miss-entry       the same jars, each class's `getJarEntry(path)` on
//                        every jar BEFORE its own (the misses `URLClassPath`
//                        pays walking to the jar that holds it; 6000 misses a
//                        round), reported per class.
//   jar-read-entry       `getInputStream(entry).readAllBytes()` on entries
//                        fetched outside the timing: the stream and the
//                        inflate.
//   jar-read-sized       `getInputStream(entry).readNBytes(size)`, as
//                        `Resource.getBytes()` reads a class for `loadClass`.
//   inflate-raw          one `new Inflater(true)` per round, `reset()` and
//                        `inflate` of each entry's raw-deflate bytes into a
//                        `byte[size]` (as `ZipFile` reuses its cached
//                        inflaters): the `Inflater` natives alone
//                        (CRATONVM_INFLATER_REUSE_BUFFERS moves this and the
//                        `*-read-*` / `*-stream` rows).
//
// Reading: `ucl-resource-stream ~= parent-get-resource + ucl-find-resource +
// url-open-stream`; `url-open-stream - jar-read-entry` is the URL connection
// and its second lookup; `jar-read-entry - inflate-raw` is the Java stream
// layers (`ZipFileInflaterInputStream`, `InflaterInputStream`,
// `ZipFileInputStream`, `RandomAccessFile` reads, the stream's `Cleaner` and
// `istreams` registration); `ucl-find-resource ~= 8.5 x jar-miss-entry/7.5 +
// jar-get-entry + the URL a hit builds`.
//
// Every row reports `<row> first ns/class=<n>` (round 0) and
// `<row> median ns/class=<n>` (median of rounds 1..ROUNDS-1). The jars are
// written by the bench itself into a temporary directory (deterministic bytes;
// nothing on `-cp`), so it runs in one step. One row only:
// `L5W73ReadPathBench <row>`.
//
// Run (interleave the environments, three repetitions, medians; $B the
// cratonvm binary, $J the JDK 25 bin directory):
//   $J/javac -d out L5W73ReadPathBench.java
//   $B --java-home $J/.. -cp out L5W73ReadPathBench                                         # default
//   env CRATONVM_PLATFORM_RESOURCE_JRT_WALK=1 $B --java-home $J/.. -cp out L5W73ReadPathBench
//   env CRATONVM_INFLATER_REUSE_BUFFERS=1 $B --java-home $J/.. -cp out L5W73ReadPathBench
//   env CRATONVM_PLATFORM_RESOURCE_JRT_WALK=1 CRATONVM_INFLATER_REUSE_BUFFERS=1 $B --java-home $J/.. -cp out L5W73ReadPathBench
//   $J/java -cp out L5W73ReadPathBench                                                        # HotSpot
// (each also with `--compatible`).
//
// stdout lines without "ns/" are deterministic (the same on every VM):
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// classes-per-round=800
// ucl-resource-stream read=800 bytes=3248940
// parent-get-resource missed=800
// ucl-find-resource found=800
// url-open-stream read=800 bytes=3248940
// jar-get-entry found=800
// jar-miss-entry missed=6000
// jar-read-entry read=800 bytes=3248940
// jar-read-sized read=800 bytes=3248940
// inflate-raw inflated=800 bytes=3248940
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.stream.Stream;
import java.util.zip.DataFormatException;
import java.util.zip.Deflater;
import java.util.zip.Inflater;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W73ReadPathBench {
    static final int JARS = 16;
    static final int CLASSES = 50;
    static final int ROUNDS = 5;
    static final int PAD = 4000;
    static final long ENTRY_TIME = 1577836800000L;

    /** `public class <name> extends java/lang/Object {}` plus an unused, compressible `CONSTANT_Utf8` (W72's generator). */
    static byte[] classBytes(String name, int pad) throws IOException {
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

    static final File[] JAR_FILES = new File[JARS];
    static final URL[] URLS = new URL[JARS];
    /** Per class (jar-major order): its jar index, entry path, uncompressed bytes and raw-deflate bytes. */
    static final List<Integer> JAR_OF = new ArrayList<>();
    static final List<String> PATHS = new ArrayList<>();
    static final List<byte[]> PLAIN = new ArrayList<>();
    static final List<byte[]> DEFLATED = new ArrayList<>();

    static byte[] rawDeflate(byte[] input) {
        Deflater deflater = new Deflater(Deflater.DEFAULT_COMPRESSION, true);
        try {
            deflater.setInput(input);
            deflater.finish();
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[8192];
            while (!deflater.finished()) {
                int n = deflater.deflate(buf);
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        } finally {
            deflater.end();
        }
    }

    static void make(Path dir) throws IOException {
        for (int j = 0; j < JARS; j++) {
            Path jar = dir.resolve("l5w73q" + j + ".jar");
            JAR_FILES[j] = jar.toFile();
            URLS[j] = jar.toUri().toURL();
            try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(jar))) {
                for (int i = 0; i < CLASSES; i++) {
                    String internal = "q" + j + "/P" + i;
                    byte[] b = classBytes(internal, PAD);
                    ZipEntry entry = new ZipEntry(internal + ".class");
                    entry.setTime(ENTRY_TIME);
                    zip.putNextEntry(entry);
                    zip.write(b);
                    zip.closeEntry();
                    JAR_OF.add(j);
                    PATHS.add(internal + ".class");
                    PLAIN.add(b);
                    DEFLATED.add(rawDeflate(b));
                }
            }
        }
    }

    /** One round of a row: the counts it reports, and the work it times. */
    interface Round {
        long[] run() throws Exception;
    }

    /** Builds a round's untimed state (open jars, fetched entries, ...) and returns the timed work. */
    interface RoundFactory {
        Round make() throws Exception;
    }

    static final List<AutoCloseable> TO_CLOSE = new ArrayList<>();

    static void closeAll() throws Exception {
        for (AutoCloseable c : TO_CLOSE) {
            c.close();
        }
        TO_CLOSE.clear();
    }

    static JarFile[] openJars() throws IOException {
        JarFile[] jars = new JarFile[JARS];
        for (int j = 0; j < JARS; j++) {
            jars[j] = new JarFile(JAR_FILES[j]);
            TO_CLOSE.add(jars[j]);
        }
        return jars;
    }

    /** `counts` labels the values a round returns (`read`, `bytes`, ...). */
    static void row(String label, String[] counts, RoundFactory factory) throws Exception {
        long[] perClass = new long[ROUNDS];
        long[] result = null;
        int classes = JARS * CLASSES;
        for (int r = 0; r < ROUNDS; r++) {
            Round round = factory.make();
            long t0 = System.nanoTime();
            result = round.run();
            perClass[r] = (System.nanoTime() - t0) / classes;
            closeAll();
        }
        long[] later = Arrays.copyOfRange(perClass, 1, ROUNDS);
        Arrays.sort(later);
        StringBuilder line = new StringBuilder(label);
        for (int k = 0; k < counts.length; k++) {
            line.append(' ').append(counts[k]).append('=').append(result[k]);
        }
        System.out.println(line);
        System.out.println(label + " first ns/class=" + perClass[0]);
        System.out.println(label + " median ns/class=" + later[later.length / 2]);
    }

    static final String[] READ_BYTES = {"read", "bytes"};

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        Path dir = Files.createTempDirectory("l5w73");
        try {
            make(dir);
            int total = JARS * CLASSES;
            System.out.println("classes-per-round=" + total);
            ClassLoader platform = ClassLoader.getPlatformClassLoader();

            if (only == null || only.equals("ucl-resource-stream")) {
                row("ucl-resource-stream", READ_BYTES, () -> {
                    URLClassLoader ucl = new URLClassLoader(URLS, platform);
                    TO_CLOSE.add(ucl);
                    return () -> {
                        long n = 0;
                        long size = 0;
                        for (String path : PATHS) {
                            try (InputStream in = ucl.getResourceAsStream(path)) {
                                if (in != null) {
                                    size += in.readAllBytes().length;
                                    n++;
                                }
                            }
                        }
                        return new long[] {n, size};
                    };
                });
            }
            if (only == null || only.equals("parent-get-resource")) {
                row("parent-get-resource", new String[] {"missed"}, () -> () -> {
                    long n = 0;
                    for (String path : PATHS) {
                        if (platform.getResource(path) == null) {
                            n++;
                        }
                    }
                    return new long[] {n};
                });
            }
            if (only == null || only.equals("ucl-find-resource")) {
                row("ucl-find-resource", new String[] {"found"}, () -> {
                    URLClassLoader ucl = new URLClassLoader(URLS, platform);
                    TO_CLOSE.add(ucl);
                    return () -> {
                        long n = 0;
                        for (String path : PATHS) {
                            if (ucl.findResource(path) != null) {
                                n++;
                            }
                        }
                        return new long[] {n};
                    };
                });
            }
            if (only == null || only.equals("url-open-stream")) {
                List<URL> urls = new ArrayList<>();
                try (URLClassLoader finder = new URLClassLoader(URLS, platform)) {
                    for (String path : PATHS) {
                        urls.add(finder.findResource(path));
                    }
                }
                row("url-open-stream", READ_BYTES, () -> () -> {
                    long n = 0;
                    long size = 0;
                    for (URL url : urls) {
                        try (InputStream in = url.openStream()) {
                            size += in.readAllBytes().length;
                            n++;
                        }
                    }
                    return new long[] {n, size};
                });
            }
            if (only == null || only.equals("jar-get-entry")) {
                row("jar-get-entry", new String[] {"found"}, () -> {
                    JarFile[] jars = openJars();
                    return () -> {
                        long n = 0;
                        for (int c = 0; c < total; c++) {
                            if (jars[JAR_OF.get(c)].getJarEntry(PATHS.get(c)) != null) {
                                n++;
                            }
                        }
                        return new long[] {n};
                    };
                });
            }
            if (only == null || only.equals("jar-miss-entry")) {
                row("jar-miss-entry", new String[] {"missed"}, () -> {
                    JarFile[] jars = openJars();
                    return () -> {
                        long n = 0;
                        for (int c = 0; c < total; c++) {
                            String path = PATHS.get(c);
                            int own = JAR_OF.get(c);
                            for (int j = 0; j < own; j++) {
                                if (jars[j].getJarEntry(path) == null) {
                                    n++;
                                }
                            }
                        }
                        return new long[] {n};
                    };
                });
            }
            if (only == null || only.equals("jar-read-entry")) {
                row("jar-read-entry", READ_BYTES, () -> {
                    JarFile[] jars = openJars();
                    JarEntry[] entries = new JarEntry[total];
                    for (int c = 0; c < total; c++) {
                        entries[c] = jars[JAR_OF.get(c)].getJarEntry(PATHS.get(c));
                    }
                    return () -> {
                        long n = 0;
                        long size = 0;
                        for (int c = 0; c < total; c++) {
                            try (InputStream in = jars[JAR_OF.get(c)].getInputStream(entries[c])) {
                                size += in.readAllBytes().length;
                                n++;
                            }
                        }
                        return new long[] {n, size};
                    };
                });
            }
            if (only == null || only.equals("jar-read-sized")) {
                row("jar-read-sized", READ_BYTES, () -> {
                    JarFile[] jars = openJars();
                    JarEntry[] entries = new JarEntry[total];
                    for (int c = 0; c < total; c++) {
                        entries[c] = jars[JAR_OF.get(c)].getJarEntry(PATHS.get(c));
                    }
                    return () -> {
                        long n = 0;
                        long size = 0;
                        for (int c = 0; c < total; c++) {
                            int len = (int) entries[c].getSize();
                            try (InputStream in = jars[JAR_OF.get(c)].getInputStream(entries[c])) {
                                size += in.readNBytes(len).length;
                                n++;
                            }
                        }
                        return new long[] {n, size};
                    };
                });
            }
            if (only == null || only.equals("inflate-raw")) {
                row("inflate-raw", new String[] {"inflated", "bytes"}, () -> () -> {
                    long n = 0;
                    long size = 0;
                    Inflater inflater = new Inflater(true);
                    try {
                        for (int c = 0; c < total; c++) {
                            byte[] out = new byte[PLAIN.get(c).length];
                            inflater.reset();
                            inflater.setInput(DEFLATED.get(c));
                            int got = 0;
                            while (got < out.length && !inflater.finished()) {
                                int k = inflater.inflate(out, got, out.length - got);
                                if (k == 0 && (inflater.needsInput() || inflater.needsDictionary())) {
                                    break;
                                }
                                got += k;
                            }
                            if (got == out.length && Arrays.equals(out, PLAIN.get(c))) {
                                n++;
                                size += got;
                            }
                        }
                    } catch (DataFormatException e) {
                        throw new IOException(e);
                    } finally {
                        inflater.end();
                    }
                    return new long[] {n, size};
                });
            }
        } finally {
            closeAll();
            try (Stream<Path> walk = Files.walk(dir)) {
                walk.sorted(Comparator.reverseOrder()).map(Path::toFile).forEach(File::delete);
            }
        }
    }
}
