// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 66, lane L5 -- timing for the shared outer open
// of `CRATONVM_NESTED_JAR_SHARED_OUTER` (see `L5W66NestedJarSharedOuter` for
// the behaviour). Each round writes a FRESH copy of one fat jar (JARS nested
// jars plus PAD `BOOT-INF/classes/` entries, so the outer central directory
// is Spring Boot-sized) at a new path, outside the timing, and a
// `URLClassLoader` over its JARS `jar:nested:` URLs (the stand-in `Handler`,
// serving from memory) loads one class that lives only in the LAST nested
// jar. That one `loadClass` walks every nested jar once, so the walk index
// BUILDS a row for each: per-spec opens map the outer file and parse its
// whole central directory JARS times; the shared open does it once. The row
// is that cost per nested jar (elapsed / JARS); a fresh path per round keeps
// the per-VM rows of earlier rounds from answering.
//
// Run:
//   javac -d out L5W66NestedJarSharedOuterBench.java
//   cratonvm --java-home <jdk25> -cp out L5W66NestedJarSharedOuterBench                                    # per-spec opens
//   CRATONVM_NESTED_JAR_SHARED_OUTER=1 cratonvm --java-home <jdk25> -cp out L5W66NestedJarSharedOuterBench # shared open
//   CRATONVM_NESTED_JAR_WALK_INDEX=0 cratonvm --java-home <jdk25> -cp out L5W66NestedJarSharedOuterBench    # every body
//   java -cp out L5W66NestedJarSharedOuterBench                                                             # HotSpot
// Interleave the binaries and take the min / median (in-VM timings swing
// between repetitions on the dev hosts). Add `CRATONVM_NESTED_JAR_BYTES_INDEX=1`
// to both CratonVM rows to see the bytes index's row builds share the open too.
//
// Prints one `round=<k> ns/op=<n>` row per round, then `min ns/op=<n>` and
// `median ns/op=<n>`, to stderr; stdout is the single line
// `loaded=1 per round`, the same on every VM. Positive control:
// `CRATONVM_DBG=access` reads `nested-jar-walk-skipped=` about
// `(JARS - 1) * ROUNDS` with the index on (shared or not), 0 with
// `CRATONVM_NESTED_JAR_WALK_INDEX=0`.

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
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W66NestedJarSharedOuterBench {
    static final int JARS = 48;
    static final int PAD = 4000;
    static final int ROUNDS = 9;

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
        out.writeByte(1); out.writeUTF("L5W66NestedJarSharedOuterBench");          // #11
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

    public static void main(String[] args) throws Exception {
        String[] inners = new String[JARS];
        ByteArrayOutputStream fat = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(fat)) {
            byte[] small = "x".getBytes("UTF-8");
            for (int p = 0; p < PAD; p++) {
                stored(zip, "BOOT-INF/classes/pad/R" + p + ".properties", small);
            }
            for (int j = 0; j < JARS; j++) {
                Map<String, byte[]> entries = new LinkedHashMap<>();
                String pkg = j == JARS - 1 ? "hit" : "miss" + j;
                entries.put(pkg + "/", new byte[0]);
                for (int c = 0; c < 20; c++) {
                    entries.put(pkg + "/C" + c + ".class", emptyClass(pkg + "/C" + c));
                }
                inners[j] = "BOOT-INF/lib/dep" + j + ".jar";
                CONTENT.put(inners[j], entries);
                stored(zip, inners[j], jar(entries));
            }
        }
        byte[] fatBytes = fat.toByteArray();
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W66NestedJarSharedOuterBench.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        Path dir = Files.createTempDirectory("l5w66-shared-outer-bench");
        long[] perOp = new long[ROUNDS];
        int loaded = 0;
        try {
            for (int r = 0; r < ROUNDS; r++) {
                Path outer = dir.resolve("outer" + r + ".jar");
                Files.write(outer, fatBytes);
                String rawPath = outer.toUri().getRawPath().replace("!", "%21");
                URL[] urls = new URL[JARS];
                for (int j = 0; j < JARS; j++) {
                    urls[j] = new URL(null, "jar:nested:" + rawPath + "/!" + inners[j] + "!/", handler);
                }
                loaded = 0;
                try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                    long t0 = System.nanoTime();
                    if (ucl.loadClass("hit.C0") != null) {
                        loaded++;
                    }
                    perOp[r] = (System.nanoTime() - t0) / JARS;
                }
                System.err.println("round=" + r + " ns/op=" + perOp[r]);
                // May fail on Windows while a per-VM mapping holds the file
                // (the bytes index); the directory is left behind then.
                try {
                    Files.deleteIfExists(outer);
                } catch (IOException ignored) {
                }
            }
        } finally {
            try {
                Files.deleteIfExists(dir);
            } catch (IOException ignored) {
            }
        }
        long[] sorted = perOp.clone();
        Arrays.sort(sorted);
        System.err.println("min ns/op=" + sorted[0]);
        System.err.println("median ns/op=" + sorted[ROUNDS / 2]);
        System.out.println("loaded=" + loaded + " per round");
    }
}
