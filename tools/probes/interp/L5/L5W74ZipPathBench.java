// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 74, lane L5 -- why `L5W73ReadPathBench`'s
// `inflate-raw` (~137 us per class on CratonVM against ~3.3 us on HotSpot 25)
// and `jar-get-entry` (~5.9 us against ~0.2 us) rows cost what they do
// (`docs/internal/proposals/interpreter/i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot-20261104.md`,
// "Progress (wave 74)").
//
// Code reading says `inflate-raw` is mostly NOT the inflater: the row checks
// every class with `Arrays.equals(out, plain)`, and `ArraysSupport.mismatch`
// ([B[BI)I is a forced intrinsic that read two `Value`s per element (8000 heap
// reads for a 4 KB class). `jar-get-entry`'s hit pays ~10 `ZipUtils.get16` /
// `get32` CEN field reads (each two `Preconditions.checkIndex` calls and an
// `Unsafe` chain) and one `ZipCoder` name compare (`Arrays.mismatch`, the same
// per-element intrinsic). These rows split them, over the same 16 jars x 50
// classes as W72 / W73 (`extends Object`, a 4000-character `Utf8`, ~4 KB each):
//
//   inflate-raw          W73's row unchanged: `reset()`, `setInput`, `inflate`
//                        into a `byte[size]`, then `Arrays.equals` against the
//                        plain bytes.
//   inflate-only         the same inflate without the `Arrays.equals` check
//                        (the bytes are summed instead): the `Inflater`
//                        natives and their Java glue alone.
//                        (CRATONVM_INFLATER_RAW_ADLER_ZERO,
//                        CRATONVM_INFLATER_REUSE_BUFFERS move this.)
//   arrays-equals-class  `Arrays.equals(plain, copy)` of each class's bytes
//                        (CRATONVM_ARRAYS_MISMATCH_BULK moves this, and
//                        `inflate-raw` by the same amount).
//   arrays-mismatch-name `Arrays.mismatch(name, 0, n, cen, off, off + n)` of
//                        each entry name against a copy at an offset: the
//                        `ZipCoder` compare's shape (CRATONVM_ARRAYS_MISMATCH_BULK).
//   zip-get-entry        16 `ZipFile`s opened outside the timing, each class's
//                        `getEntry(path)` on its own: one CEN hit without the
//                        `JarFile` layer (CRATONVM_ZIP_UTILS_INTRINSICS,
//                        CRATONVM_ARRAYS_MISMATCH_BULK).
//   jar-get-entry        the same through `JarFile.getJarEntry` (W73's row):
//                        `jar-get-entry - zip-get-entry` is the `JarFile` layer
//                        (`isMultiRelease`, `JarFileEntry`).
//   zip-miss-entry       each class's `getEntry(path)` on every `ZipFile`
//                        before its own (6000 misses a round), reported per
//                        class: the hash walk with no CEN field read.
//   jar-read-sized       W73's row: `JarFile.getInputStream(entry)
//                        .readNBytes(size)`, the read `loadClass` does (every
//                        switch above moves it a little).
//
// Every row reports `<row> first ns/class=<n>` (round 0) and
// `<row> median ns/class=<n>` (median of rounds 1..ROUNDS-1). The jars are
// written by the bench itself into a temporary directory (deterministic bytes;
// nothing on `-cp`), so it runs in one step. One row only:
// `L5W74ZipPathBench <row>`.
//
// Run (interleave the environments, three repetitions, medians; $B the
// cratonvm binary, $J the JDK 25 bin directory):
//   $J/javac -d out L5W74ZipPathBench.java
//   $B --java-home $J/.. -cp out L5W74ZipPathBench                                       # default
//   env CRATONVM_ARRAYS_MISMATCH_BULK=1 $B --java-home $J/.. -cp out L5W74ZipPathBench
//   env CRATONVM_ZIP_UTILS_INTRINSICS=1 $B --java-home $J/.. -cp out L5W74ZipPathBench
//   env CRATONVM_INFLATER_RAW_ADLER_ZERO=1 $B --java-home $J/.. -cp out L5W74ZipPathBench
//   env CRATONVM_ARRAYS_MISMATCH_BULK=1 CRATONVM_ZIP_UTILS_INTRINSICS=1 CRATONVM_INFLATER_RAW_ADLER_ZERO=1 $B --java-home $J/.. -cp out L5W74ZipPathBench
//   $J/java -cp out L5W74ZipPathBench                                                      # HotSpot
// (each also with `--compatible`).
//
// stdout lines without "ns/" are deterministic (the same on every VM):
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// classes-per-round=800
// inflate-raw inflated=800 bytes=3248940
// inflate-only inflated=800 bytes=3248940
// arrays-equals-class equal=800 bytes=3248940
// arrays-mismatch-name matched=800 differed=800
// zip-get-entry found=800
// jar-get-entry found=800
// zip-miss-entry missed=6000
// jar-read-sized read=800 bytes=3248940
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
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
import java.util.zip.ZipFile;
import java.util.zip.ZipOutputStream;

public class L5W74ZipPathBench {
    static final int JARS = 16;
    static final int CLASSES = 50;
    static final int ROUNDS = 5;
    static final int PAD = 4000;
    static final long ENTRY_TIME = 1577836800000L;
    /** Bytes in front of each name's copy in `arrays-mismatch-name` (a CEN header's size). */
    static final int NAME_OFFSET = 46;

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
    /** Per class (jar-major order): its jar index, entry path, uncompressed bytes, a copy of them and raw-deflate bytes. */
    static final List<Integer> JAR_OF = new ArrayList<>();
    static final List<String> PATHS = new ArrayList<>();
    static final List<byte[]> PLAIN = new ArrayList<>();
    static final List<byte[]> COPY = new ArrayList<>();
    static final List<byte[]> DEFLATED = new ArrayList<>();
    /** Per class: the entry name's UTF-8 bytes, and a "CEN" buffer holding them after NAME_OFFSET bytes. */
    static final List<byte[]> NAMES = new ArrayList<>();
    static final List<byte[]> CENS = new ArrayList<>();

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
            Path jar = dir.resolve("l5w74z" + j + ".jar");
            JAR_FILES[j] = jar.toFile();
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
                    COPY.add(b.clone());
                    DEFLATED.add(rawDeflate(b));
                    byte[] name = (internal + ".class").getBytes(StandardCharsets.UTF_8);
                    byte[] cen = new byte[NAME_OFFSET + name.length + 8];
                    System.arraycopy(name, 0, cen, NAME_OFFSET, name.length);
                    NAMES.add(name);
                    CENS.add(cen);
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

    static ZipFile[] openZips() throws IOException {
        ZipFile[] zips = new ZipFile[JARS];
        for (int j = 0; j < JARS; j++) {
            zips[j] = new ZipFile(JAR_FILES[j]);
            TO_CLOSE.add(zips[j]);
        }
        return zips;
    }

    static JarFile[] openJars() throws IOException {
        JarFile[] jars = new JarFile[JARS];
        for (int j = 0; j < JARS; j++) {
            jars[j] = new JarFile(JAR_FILES[j]);
            TO_CLOSE.add(jars[j]);
        }
        return jars;
    }

    /** `counts` labels the values a round returns (`found`, `bytes`, ...). */
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

    /** Inflates every class's raw-deflate bytes on one `Inflater`; `check` compares each with `Arrays.equals`. */
    static long[] inflateAll(int total, boolean check) throws IOException {
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
                boolean ok = got == out.length && (!check || Arrays.equals(out, PLAIN.get(c)));
                if (ok) {
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
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        Path dir = Files.createTempDirectory("l5w74");
        try {
            make(dir);
            int total = JARS * CLASSES;
            System.out.println("classes-per-round=" + total);

            if (only == null || only.equals("inflate-raw")) {
                row("inflate-raw", new String[] {"inflated", "bytes"}, () -> () -> inflateAll(total, true));
            }
            if (only == null || only.equals("inflate-only")) {
                row("inflate-only", new String[] {"inflated", "bytes"}, () -> () -> inflateAll(total, false));
            }
            if (only == null || only.equals("arrays-equals-class")) {
                row("arrays-equals-class", new String[] {"equal", "bytes"}, () -> () -> {
                    long n = 0;
                    long size = 0;
                    for (int c = 0; c < total; c++) {
                        byte[] a = PLAIN.get(c);
                        if (Arrays.equals(a, COPY.get(c))) {
                            n++;
                            size += a.length;
                        }
                    }
                    return new long[] {n, size};
                });
            }
            if (only == null || only.equals("arrays-mismatch-name")) {
                row("arrays-mismatch-name", new String[] {"matched", "differed"}, () -> () -> {
                    long same = 0;
                    long differ = 0;
                    for (int c = 0; c < total; c++) {
                        byte[] name = NAMES.get(c);
                        int len = name.length;
                        // The entry's own name: a match.
                        if (Arrays.mismatch(name, 0, len, CENS.get(c), NAME_OFFSET, NAME_OFFSET + len) == -1) {
                            same++;
                        }
                        // The next entry's name (wrapping): a first difference after the shared prefix.
                        byte[] other = CENS.get((c + 1) % total);
                        if (Arrays.mismatch(name, 0, len, other, NAME_OFFSET, NAME_OFFSET + len) >= 0) {
                            differ++;
                        }
                    }
                    return new long[] {same, differ};
                });
            }
            if (only == null || only.equals("zip-get-entry")) {
                row("zip-get-entry", new String[] {"found"}, () -> {
                    ZipFile[] zips = openZips();
                    return () -> {
                        long n = 0;
                        for (int c = 0; c < total; c++) {
                            if (zips[JAR_OF.get(c)].getEntry(PATHS.get(c)) != null) {
                                n++;
                            }
                        }
                        return new long[] {n};
                    };
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
            if (only == null || only.equals("zip-miss-entry")) {
                row("zip-miss-entry", new String[] {"missed"}, () -> {
                    ZipFile[] zips = openZips();
                    return () -> {
                        long n = 0;
                        for (int c = 0; c < total; c++) {
                            String path = PATHS.get(c);
                            int own = JAR_OF.get(c);
                            for (int j = 0; j < own; j++) {
                                if (zips[j].getEntry(path) == null) {
                                    n++;
                                }
                            }
                        }
                        return new long[] {n};
                    };
                });
            }
            if (only == null || only.equals("jar-read-sized")) {
                row("jar-read-sized", new String[] {"read", "bytes"}, () -> {
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
        } finally {
            closeAll();
            try (Stream<Path> walk = Files.walk(dir)) {
                walk.sorted(Comparator.reverseOrder()).map(Path::toFile).forEach(File::delete);
            }
        }
    }
}
