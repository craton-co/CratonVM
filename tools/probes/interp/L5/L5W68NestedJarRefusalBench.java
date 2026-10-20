// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 68, lane L5 -- timing for
// `CRATONVM_NESTED_JAR_REFUSAL_STAMP` (see `L5W68NestedJarRefusalRetry` for
// the behaviour): the per-ask cost of the nested-jar walk BEFORE and AFTER
// its fat jar was caught half written once. One fat jar (PAD
// `BOOT-INF/classes/` entries and JARS nested jars) on disk, one
// `URLClassLoader` over its JARS `jar:nested:` URLs (the stand-in `Handler`,
// serving from memory); each op is a `getResource` of a name only the LAST
// nested jar holds, so it walks every nested jar: with the walk index the
// first JARS - 1 are skips. Then the jar is truncated to half for ONE walk
// (every row is rebuilt from the half jar and refused), rewritten whole (one
// more pad entry, so a new length), and the same ops are timed again.
//
// Without the switch every refused row runs the JDK body for the rest of the
// VM: `after` is the no-index cost (wave-54/66 benches: several times
// `before`). With it each refused row is rebuilt on its first ask past the
// 100 ms retry interval: `after` returns to about `before`.
//
// Run:
//   javac -d out L5W68NestedJarRefusalBench.java
//   cratonvm --java-home <jdk25> -cp out L5W68NestedJarRefusalBench                                      # refusals kept
//   CRATONVM_NESTED_JAR_REFUSAL_STAMP=1 cratonvm --java-home <jdk25> -cp out L5W68NestedJarRefusalBench  # refusals retried
//   CRATONVM_NESTED_JAR_WALK_INDEX=0 cratonvm --java-home <jdk25> -cp out L5W68NestedJarRefusalBench     # every body
//   java -cp out L5W68NestedJarRefusalBench                                                               # HotSpot
// Interleave the binaries and take the medians (in-VM timings swing between
// repetitions on the dev hosts).
//
// Prints `phase=<before|after> rep=<k> ns/op=<n>` rows, each phase's `min` and
// `median`, and `after/before median ratio=<r>` to stderr; stdout is the
// found counts, the same on every VM. Positive control: `CRATONVM_DBG=access`
// reads `nested-jar-walk-skipped=` about `2 * REPS * OPS * (JARS - 1)` with
// the switch, about half that without it.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// found-before=1400
// found-after=1400
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.URLConnection;
import java.net.URLStreamHandler;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.Arrays;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W68NestedJarRefusalBench {
    static final int JARS = 48;
    static final int PAD = 2000;
    static final int REPS = 7;
    static final int OPS = 200;

    /** inner jar name -> entry name -> bytes. */
    static final Map<String, Map<String, byte[]>> CONTENT = new HashMap<>();

    public static URLConnection open(URL url) throws IOException {
        String file = url.getFile();
        int sep = file.indexOf("!/");
        String nested = file.substring(0, sep);
        String entry = file.substring(sep + 2);
        String inner = nested.substring(nested.indexOf("/!") + 2);
        Map<String, byte[]> jar = CONTENT.get(inner);
        byte[] bytes = jar == null ? null : jar.get(entry);
        if (bytes == null && jar != null) {
            bytes = jar.get(entry + "/");
        }
        return new Conn(url, bytes);
    }

    static final class Conn extends URLConnection {
        final byte[] bytes;

        Conn(URL url, byte[] bytes) {
            super(url);
            this.bytes = bytes;
        }

        @Override
        public void connect() {
        }

        @Override
        public InputStream getInputStream() throws IOException {
            if (bytes == null) {
                throw new FileNotFoundException("Jar file or entry not found");
            }
            return new ByteArrayInputStream(bytes);
        }

        @Override
        public int getContentLength() {
            return bytes == null ? -1 : bytes.length;
        }
    }

    static final class HandlerLoader extends ClassLoader {
        HandlerLoader(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(byte[] b) {
            return defineClass(null, b, 0, b.length);
        }
    }

    /** `public class <name> extends Object {}` (version 49). */
    static byte[] emptyClass(String name) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(5);
        out.writeByte(1); out.writeUTF(name);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF("java/lang/Object");
        out.writeByte(7); out.writeShort(3);
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

    /** Spring Boot's handler's class name, `openConnection` calling `open`. */
    static byte[] handlerClass() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(17);
        out.writeByte(1); out.writeUTF("org/springframework/boot/loader/net/protocol/jar/Handler"); // #1
        out.writeByte(7); out.writeShort(1);                                       // #2
        out.writeByte(1); out.writeUTF("java/net/URLStreamHandler");               // #3
        out.writeByte(7); out.writeShort(3);                                       // #4
        out.writeByte(1); out.writeUTF("<init>");                                  // #5
        out.writeByte(1); out.writeUTF("()V");                                     // #6
        out.writeByte(12); out.writeShort(5); out.writeShort(6);                   // #7
        out.writeByte(10); out.writeShort(4); out.writeShort(7);                   // #8
        out.writeByte(1); out.writeUTF("openConnection");                          // #9
        out.writeByte(1); out.writeUTF("(Ljava/net/URL;)Ljava/net/URLConnection;"); // #10
        out.writeByte(1); out.writeUTF("L5W68NestedJarRefusalBench");          // #11
        out.writeByte(7); out.writeShort(11);                                      // #12
        out.writeByte(1); out.writeUTF("open");                                    // #13
        out.writeByte(12); out.writeShort(13); out.writeShort(10);                 // #14
        out.writeByte(10); out.writeShort(12); out.writeShort(14);                 // #15
        out.writeByte(1); out.writeUTF("Code");                                    // #16
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(2);
        out.writeShort(0x01); out.writeShort(5); out.writeShort(6); out.writeShort(1);
        out.writeShort(16); out.writeInt(17);
        out.writeShort(1); out.writeShort(1); out.writeInt(5);
        out.writeByte(0x2A); out.writeByte(0xB7); out.writeShort(8); out.writeByte(0xB1);
        out.writeShort(0); out.writeShort(0);
        out.writeShort(0x01); out.writeShort(9); out.writeShort(10); out.writeShort(1);
        out.writeShort(16); out.writeInt(17);
        out.writeShort(1); out.writeShort(2); out.writeInt(5);
        out.writeByte(0x2B); out.writeByte(0xB8); out.writeShort(15); out.writeByte(0xB0);
        out.writeShort(0); out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    static byte[] jar(Map<String, byte[]> entries) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            for (Map.Entry<String, byte[]> e : entries.entrySet()) {
                zip.putNextEntry(new ZipEntry(e.getKey()));
                zip.write(e.getValue());
                zip.closeEntry();
            }
        }
        return bytes.toByteArray();
    }

    static void stored(ZipOutputStream zip, String name, byte[] data) throws IOException {
        ZipEntry entry = new ZipEntry(name);
        entry.setMethod(ZipEntry.STORED);
        entry.setSize(data.length);
        entry.setCompressedSize(data.length);
        CRC32 crc = new CRC32();
        crc.update(data);
        entry.setCrc(crc.getValue());
        zip.putNextEntry(entry);
        zip.write(data);
        zip.closeEntry();
    }

    /** The fat jar: PAD `BOOT-INF/classes/` entries (`extra` adds one), then JARS nested jars. */
    static byte[] fatJar(String[] inners, boolean extra) throws IOException {
        ByteArrayOutputStream fat = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(fat)) {
            byte[] small = "x".getBytes("UTF-8");
            for (int p = 0; p < PAD + (extra ? 1 : 0); p++) {
                stored(zip, "BOOT-INF/classes/pad/R" + p + ".properties", small);
            }
            for (int j = 0; j < JARS; j++) {
                stored(zip, inners[j], jar(CONTENT.get(inners[j])));
            }
        }
        return fat.toByteArray();
    }

    /** OPS asks of a name only the LAST nested jar holds: each walks JARS loaders. */
    static long[] phase(URLClassLoader ucl, String label) {
        long[] perOp = new long[REPS];
        int found = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < OPS; i++) {
                if (ucl.getResource("hit/C" + (i % 20) + ".class") != null) {
                    found++;
                }
            }
            perOp[r] = (System.nanoTime() - t0) / OPS;
            System.err.println("phase=" + label + " rep=" + r + " ns/op=" + perOp[r]);
        }
        System.out.println("found-" + label + "=" + found);
        long[] sorted = perOp.clone();
        Arrays.sort(sorted);
        System.err.println("phase=" + label + " min ns/op=" + sorted[0]);
        System.err.println("phase=" + label + " median ns/op=" + sorted[REPS / 2]);
        return sorted;
    }

    public static void main(String[] args) throws Exception {
        String[] inners = new String[JARS];
        for (int j = 0; j < JARS; j++) {
            Map<String, byte[]> entries = new LinkedHashMap<>();
            String pkg = j == JARS - 1 ? "hit" : "miss" + j;
            entries.put(pkg + "/", new byte[0]);
            for (int c = 0; c < 20; c++) {
                entries.put(pkg + "/C" + c + ".class", emptyClass(pkg + "/C" + c));
            }
            inners[j] = "BOOT-INF/lib/dep" + j + ".jar";
            CONTENT.put(inners[j], entries);
        }
        byte[] whole = fatJar(inners, false);
        byte[] rewritten = fatJar(inners, true);
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W68NestedJarRefusalBench.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        Path dir = Files.createTempDirectory("l5w68-refusal-bench");
        Path outer = dir.resolve("app.jar");
        try {
            Files.write(outer, whole);
            String rawPath = outer.toUri().getRawPath().replace("!", "%21");
            URL[] urls = new URL[JARS];
            for (int j = 0; j < JARS; j++) {
                urls[j] = new URL(null, "jar:nested:" + rawPath + "/!" + inners[j] + "!/", handler);
            }
            try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                long[] before = phase(ucl, "before");

                // Caught mid-rewrite: half the jar on disk for one walk, so
                // every walk row is rebuilt from it and refused.
                try (FileChannel ch = FileChannel.open(outer, StandardOpenOption.WRITE)) {
                    ch.truncate(whole.length / 2);
                }
                ucl.getResource("hit/C0.class");
                Thread.sleep(250);
                Files.write(outer, rewritten, StandardOpenOption.TRUNCATE_EXISTING, StandardOpenOption.WRITE);
                Thread.sleep(250);

                long[] after = phase(ucl, "after");
                System.err.println("after/before median ratio="
                        + String.format("%.2f", (double) after[REPS / 2] / Math.max(1, before[REPS / 2])));
            }
        } finally {
            Files.deleteIfExists(outer);
            Files.deleteIfExists(dir);
        }
    }
}
