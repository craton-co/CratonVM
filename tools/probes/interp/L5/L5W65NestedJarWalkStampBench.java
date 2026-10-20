// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 65, lane L5 -- timing for the stamped walk
// index (`CRATONVM_NESTED_JAR_WALK_INDEX_STAMP`; see
// `L5W65NestedJarWalkRewrite` for the behaviour). A `URLClassLoader` over
// JARS Spring Boot-shaped `jar:nested:` URLs (the stand-in `Handler`, serving
// from memory) loads CLASSES classes that live only in the LAST nested jar,
// so every `loadClass` walks JARS - 1 steps the index skips. With the stamp
// each skipped step also `stat`s the outer fat jar; the row is the cost of
// that per defined class.
//
// Run:
//   javac -d out L5W65NestedJarWalkStampBench.java
//   cratonvm --java-home <jdk25> -cp out L5W65NestedJarWalkStampBench                                       # index, no stamp
//   CRATONVM_NESTED_JAR_WALK_INDEX_STAMP=1 cratonvm --java-home <jdk25> -cp out L5W65NestedJarWalkStampBench # stamped
//   CRATONVM_NESTED_JAR_WALK_INDEX=0 cratonvm --java-home <jdk25> -cp out L5W65NestedJarWalkStampBench       # every body
//   java -cp out L5W65NestedJarWalkStampBench                                                                # HotSpot
// Interleave the binaries and take the min / median (in-VM timings swing
// between repetitions on the dev hosts).
//
// Prints one `round=<k> ns/op=<n>` row per round, then `min ns/op=<n>` and
// `median ns/op=<n>`, to stderr; stdout is the single line
// `loaded=<CLASSES> per round`, the same on every VM. Positive control:
// `CRATONVM_DBG=access` reads `nested-jar-walk-skipped=` about
// `(JARS - 1) * CLASSES * ROUNDS` with the index on (stamped or not), 0 with
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

public class L5W65NestedJarWalkStampBench {
    static final int JARS = 16;
    static final int CLASSES = 200;
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
        out.writeByte(1); out.writeUTF("L5W65NestedJarWalkStampBench");            // #11
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
        Path dir = Files.createTempDirectory("l5w65-walk-bench");
        Path outer = dir.resolve("outer.jar");
        String[] inners = new String[JARS];
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(outer))) {
            for (int j = 0; j < JARS; j++) {
                Map<String, byte[]> entries = new LinkedHashMap<>();
                String pkg = j == JARS - 1 ? "hit" : "miss" + j;
                entries.put(pkg + "/", new byte[0]);
                int count = j == JARS - 1 ? CLASSES : 20;
                for (int c = 0; c < count; c++) {
                    entries.put(pkg + "/C" + c + ".class", emptyClass(pkg + "/C" + c));
                }
                inners[j] = "BOOT-INF/lib/dep" + j + ".jar";
                CONTENT.put(inners[j], entries);
                stored(zip, inners[j], jar(entries));
            }
        }
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W65NestedJarWalkStampBench.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        String rawPath = outer.toUri().getRawPath().replace("!", "%21");
        URL[] urls = new URL[JARS];
        for (int j = 0; j < JARS; j++) {
            urls[j] = new URL(null, "jar:nested:" + rawPath + "/!" + inners[j] + "!/", handler);
        }
        long[] perOp = new long[ROUNDS];
        int loaded = 0;
        try {
            for (int r = 0; r < ROUNDS; r++) {
                loaded = 0;
                try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                    long t0 = System.nanoTime();
                    for (int c = 0; c < CLASSES; c++) {
                        if (ucl.loadClass("hit.C" + c) != null) {
                            loaded++;
                        }
                    }
                    perOp[r] = (System.nanoTime() - t0) / CLASSES;
                }
                System.err.println("round=" + r + " ns/op=" + perOp[r]);
            }
        } finally {
            Files.deleteIfExists(outer);
            Files.deleteIfExists(dir);
        }
        long[] sorted = perOp.clone();
        Arrays.sort(sorted);
        System.err.println("min ns/op=" + sorted[0]);
        System.err.println("median ns/op=" + sorted[ROUNDS / 2]);
        System.out.println("loaded=" + loaded + " per round");
    }
}
