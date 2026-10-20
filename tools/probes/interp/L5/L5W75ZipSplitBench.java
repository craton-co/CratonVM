// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 75, lane L5 -- where a `ZipFile.getEntry` hit and
// a class-path entry read (`L5W74ZipPathBench`'s `jar-read-sized`, ~60 us per
// class under the JIT against ~15-20 us for `inflate-only`) spend their time
// (`docs/internal/proposals/interpreter/i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot-20261104.md`,
// "Progress (wave 75)"; `interpreter-i74-L5-proposal-zip-cen-accessor-intrinsics-20261107-DONE-20261108.md`).
//
// Switches it A/Bs (both opt-in, default OFF, the same answers either way):
//   CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS=1  the ZipUtils CEN / LOC accessors
//       (CENFLG, CENNAM, ..., LOCEXT) are Intrinsic natives.
//   CRATONVM_ZIP_CODER_COMPARE_INTRINSIC=1  UTF8ZipCoder.compare of an ASCII
//       name runs in place, without the encode.
//
// Same 16 jars x 50 classes as W72 / W73 / W74 (`extends Object`, a
// 4000-character Utf8, ~4 KB each, deflated). Rows, each reported per class:
//
//   zip-get-entry        ZipFile.getEntry(path) hit (W74's row): the hash
//                        walk, the name compare, ~10 CEN field reads, the
//                        ZipEntry. Both switches move it.
//   zip-miss-entry       getEntry(path) on every ZipFile before the class's
//                        own (6000 misses a round): the hash walk only. A
//                        control: neither switch may move it.
//   jar-get-entry        the same hit through JarFile.getJarEntry.
//   zip-open-close-last  getEntry(path) then getInputStream(that entry) and
//                        close(), no read: ZipFile.getInputStream finds the
//                        entry by `lastEntryName` (no second lookup).
//   zip-open-close       getInputStream(entry) of an entry fetched before the
//                        round, and close(): the second CEN lookup (name
//                        compare again), ZipFileInputStream (CENSIZ / CENLEN /
//                        CENOFF), ZipFileInflaterInputStream (cached Inflater,
//                        input buffer, Cleaner registration), the istreams
//                        WeakHashMap add, and close()'s two removes, Cleaner
//                        clean and Inflater.reset().
//   jar-open-close       the same through JarFile.getInputStream.
//   zip-read-sized       zip-open-close plus readNBytes(size): the read
//                        loadClass's Resource.getBytes does, without JarFile.
//   jar-read-sized       W74's row: JarFile.getInputStream(entry)
//                        .readNBytes(size).
//   inflate-only         W74's row: one raw Inflater, reset / setInput /
//                        inflate per class (the zlib natives and their glue).
//   raf-seek-read        what ZipFile$Source reads per entry: a seek and a
//                        30-byte read (the LOC header), a seek and a 1536-byte
//                        read (the compressed bytes), on a RandomAccessFile
//                        under `synchronized`.
//   cleaner-clean        Cleaner.register(obj, action).clean() (the
//                        ZipFileInflaterInputStream's Cleaner, registered and
//                        cleaned once per read).
//   weak-set-churn       a Collections.newSetFromMap(new WeakHashMap<>())
//                        add and two removes (ZipFile's `istreams` per read).
//   inflater-cache       ZipFile's cached-Inflater round trip: synchronized
//                        ArrayDeque poll, Inflater.reset(), synchronized add.
//   filter-read-nbytes   InputStream.readNBytes(size) through a plain
//                        FilterInputStream over a ByteArrayInputStream: the
//                        generic readNBytes glue without any zip code.
//
// Read: zip-read-sized ~= zip-open-close + inflate-only + raf-seek-read +
// filter-read-nbytes; zip-open-close - zip-open-close-last is the second
// lookup; zip-open-close-last against cleaner-clean + weak-set-churn +
// inflater-cache is the stream objects' share. Whatever is left is the
// JDK's Java between them.
//
// stdout: per row one deterministic "<row> <counts>" line (no "ns/") and one
// timing line "<row> <n> ns/op (median of rounds 1..ROUNDS-1, per class)".
// One row only: `L5W75ZipSplitBench <row>`. The jars are written by the bench
// into a temporary directory (one step; nothing on -cp).
//
// A/B ($B the cratonvm binary, $J the JDK 25 bin directory; the same binary,
// interleaved, three repetitions, medians; with and without --nojit):
//   $J/javac -d out L5W75ZipSplitBench.java
//   $B --java-home $J/.. -cp out L5W75ZipSplitBench                                                # off
//   env CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS=1 $B --java-home $J/.. -cp out L5W75ZipSplitBench     # accessors
//   env CRATONVM_ZIP_CODER_COMPARE_INTRINSIC=1 $B --java-home $J/.. -cp out L5W75ZipSplitBench     # compare
//   env CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS=1 CRATONVM_ZIP_CODER_COMPARE_INTRINSIC=1 $B --java-home $J/.. -cp out L5W75ZipSplitBench
//   $J/java -cp out L5W75ZipSplitBench                                                             # HotSpot
// (each also with --compatible). Expected: the switches lower zip-get-entry,
// jar-get-entry, zip-open-close (two lookups' worth of compare) and the
// *-read-sized rows; zip-miss-entry, inflate-only, raf-seek-read and the
// three Java-collection rows do not move.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// classes-per-round=800
// zip-get-entry found=800
// zip-miss-entry missed=6000
// jar-get-entry found=800
// zip-open-close-last opened=800
// zip-open-close opened=800
// jar-open-close opened=800
// zip-read-sized read=800 bytes=3248940
// jar-read-sized read=800 bytes=3248940
// inflate-only inflated=800 bytes=3248940
// raf-seek-read reads=1600 bytes=1252800
// cleaner-clean cleaned=800
// weak-set-churn added=800 left=0
// inflater-cache polled=800
// filter-read-nbytes read=800 bytes=3248940
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.FilterInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.RandomAccessFile;
import java.lang.ref.Cleaner;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Comparator;
import java.util.Deque;
import java.util.List;
import java.util.Set;
import java.util.WeakHashMap;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.stream.Stream;
import java.util.zip.DataFormatException;
import java.util.zip.Deflater;
import java.util.zip.Inflater;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipOutputStream;

public class L5W75ZipSplitBench {
    static final int JARS = 16;
    static final int CLASSES = 50;
    static final int ROUNDS = 7;
    static final int PAD = 4000;
    static final long ENTRY_TIME = 1577836800000L;
    static final int LOC_BYTES = 30;
    static final int DATA_BYTES = 1536;

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
            Path jar = dir.resolve("l5w75z" + j + ".jar");
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
                    DEFLATED.add(rawDeflate(b));
                }
            }
        }
    }

    interface Round {
        long[] run() throws Exception;
    }

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

    static ZipEntry[] entriesOf(ZipFile[] zips) {
        ZipEntry[] entries = new ZipEntry[JARS * CLASSES];
        for (int c = 0; c < entries.length; c++) {
            entries[c] = zips[JAR_OF.get(c)].getEntry(PATHS.get(c));
        }
        return entries;
    }

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
        System.out.println(label + " " + later[later.length / 2]
                + " ns/op (median of rounds 1.." + (ROUNDS - 1) + ", per class)");
    }

    static boolean wanted(String only, String row) {
        return only == null || only.equals(row);
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        Path dir = Files.createTempDirectory("l5w75");
        try {
            make(dir);
            int total = JARS * CLASSES;
            System.out.println("classes-per-round=" + total);

            if (wanted(only, "zip-get-entry")) {
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
            if (wanted(only, "zip-miss-entry")) {
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
            if (wanted(only, "jar-get-entry")) {
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
            if (wanted(only, "zip-open-close-last")) {
                row("zip-open-close-last", new String[] {"opened"}, () -> {
                    ZipFile[] zips = openZips();
                    return () -> {
                        long n = 0;
                        for (int c = 0; c < total; c++) {
                            ZipFile zip = zips[JAR_OF.get(c)];
                            ZipEntry entry = zip.getEntry(PATHS.get(c));
                            try (InputStream in = zip.getInputStream(entry)) {
                                if (in != null) {
                                    n++;
                                }
                            }
                        }
                        return new long[] {n};
                    };
                });
            }
            if (wanted(only, "zip-open-close")) {
                row("zip-open-close", new String[] {"opened"}, () -> {
                    ZipFile[] zips = openZips();
                    ZipEntry[] entries = entriesOf(zips);
                    return () -> {
                        long n = 0;
                        for (int c = 0; c < total; c++) {
                            try (InputStream in = zips[JAR_OF.get(c)].getInputStream(entries[c])) {
                                if (in != null) {
                                    n++;
                                }
                            }
                        }
                        return new long[] {n};
                    };
                });
            }
            if (wanted(only, "jar-open-close")) {
                row("jar-open-close", new String[] {"opened"}, () -> {
                    JarFile[] jars = openJars();
                    JarEntry[] entries = new JarEntry[total];
                    for (int c = 0; c < total; c++) {
                        entries[c] = jars[JAR_OF.get(c)].getJarEntry(PATHS.get(c));
                    }
                    return () -> {
                        long n = 0;
                        for (int c = 0; c < total; c++) {
                            try (InputStream in = jars[JAR_OF.get(c)].getInputStream(entries[c])) {
                                if (in != null) {
                                    n++;
                                }
                            }
                        }
                        return new long[] {n};
                    };
                });
            }
            if (wanted(only, "zip-read-sized")) {
                row("zip-read-sized", new String[] {"read", "bytes"}, () -> {
                    ZipFile[] zips = openZips();
                    ZipEntry[] entries = entriesOf(zips);
                    return () -> {
                        long n = 0;
                        long size = 0;
                        for (int c = 0; c < total; c++) {
                            int len = (int) entries[c].getSize();
                            try (InputStream in = zips[JAR_OF.get(c)].getInputStream(entries[c])) {
                                size += in.readNBytes(len).length;
                                n++;
                            }
                        }
                        return new long[] {n, size};
                    };
                });
            }
            if (wanted(only, "jar-read-sized")) {
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
            if (wanted(only, "inflate-only")) {
                row("inflate-only", new String[] {"inflated", "bytes"}, () -> () -> {
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
                            if (got == out.length) {
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
            if (wanted(only, "raf-seek-read")) {
                row("raf-seek-read", new String[] {"reads", "bytes"}, () -> {
                    RandomAccessFile[] files = new RandomAccessFile[JARS];
                    for (int j = 0; j < JARS; j++) {
                        files[j] = new RandomAccessFile(JAR_FILES[j], "r");
                        TO_CLOSE.add(files[j]);
                    }
                    byte[] loc = new byte[LOC_BYTES];
                    byte[] data = new byte[DATA_BYTES];
                    return () -> {
                        long reads = 0;
                        long bytes = 0;
                        for (int c = 0; c < total; c++) {
                            RandomAccessFile f = files[JAR_OF.get(c)];
                            long span = f.length() - LOC_BYTES - DATA_BYTES;
                            long pos = ((long) (c % CLASSES) * 1409L) % span;
                            synchronized (f) {
                                f.seek(pos);
                                f.readFully(loc, 0, LOC_BYTES);
                            }
                            reads++;
                            bytes += LOC_BYTES;
                            synchronized (f) {
                                f.seek(pos + LOC_BYTES);
                                f.readFully(data, 0, DATA_BYTES);
                            }
                            reads++;
                            bytes += DATA_BYTES;
                        }
                        return new long[] {reads, bytes};
                    };
                });
            }
            if (wanted(only, "cleaner-clean")) {
                Cleaner cleaner = Cleaner.create();
                row("cleaner-clean", new String[] {"cleaned"}, () -> () -> {
                    long[] cleaned = new long[1];
                    Runnable action = () -> cleaned[0]++;
                    for (int c = 0; c < total; c++) {
                        Object owner = new Object();
                        Cleaner.Cleanable cleanable = cleaner.register(owner, action);
                        cleanable.clean();
                    }
                    return new long[] {cleaned[0]};
                });
            }
            if (wanted(only, "weak-set-churn")) {
                row("weak-set-churn", new String[] {"added", "left"}, () -> () -> {
                    Set<Object> streams = Collections.newSetFromMap(new WeakHashMap<>());
                    long added = 0;
                    for (int c = 0; c < total; c++) {
                        Object stream = new Object();
                        synchronized (streams) {
                            if (streams.add(stream)) {
                                added++;
                            }
                        }
                        synchronized (streams) {
                            streams.remove(stream);
                        }
                        synchronized (streams) {
                            streams.remove(stream);
                        }
                    }
                    return new long[] {added, streams.size()};
                });
            }
            if (wanted(only, "inflater-cache")) {
                row("inflater-cache", new String[] {"polled"}, () -> {
                    Deque<Inflater> cache = new ArrayDeque<>();
                    cache.add(new Inflater(true));
                    TO_CLOSE.add(() -> {
                        Inflater inf;
                        while ((inf = cache.poll()) != null) {
                            inf.end();
                        }
                    });
                    return () -> {
                        long polled = 0;
                        for (int c = 0; c < total; c++) {
                            Inflater inf;
                            synchronized (cache) {
                                inf = cache.poll();
                            }
                            if (inf == null) {
                                inf = new Inflater(true);
                            } else {
                                polled++;
                            }
                            synchronized (cache) {
                                inf.reset();
                                cache.add(inf);
                            }
                        }
                        return new long[] {polled};
                    };
                });
            }
            if (wanted(only, "filter-read-nbytes")) {
                row("filter-read-nbytes", new String[] {"read", "bytes"}, () -> () -> {
                    long n = 0;
                    long size = 0;
                    for (int c = 0; c < total; c++) {
                        byte[] plain = PLAIN.get(c);
                        try (InputStream in = new FilterInputStream(new ByteArrayInputStream(plain)) {}) {
                            size += in.readNBytes(plain.length).length;
                            n++;
                        }
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
