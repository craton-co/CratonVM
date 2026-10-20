// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 68, lane L5 -- timing for the nested-jar bytes
// index (`CRATONVM_NESTED_JAR_BYTES_INDEX`, default off) with the outer index
// on (`CRATONVM_NESTED_JAR_OUTER_INDEX`, default on since wave 67), with and
// without the wave-68 directory-only rows (`CRATONVM_NESTED_JAR_BYTES_DIRECTORY`:
// a row over a STORED nested jar keeps only its central directory and reads
// each class from the outer file at the ask, instead of owning a copy of the
// whole nested jar;
// `docs/internal/fixed-bugs/interpreter-i67-L5-proposal-a-bytes-index-row-without-the-nested-bytes-KEPT-OFF-20261031.md`).
//
// One fat jar on disk: JARS stored nested jars of CLASSES deflated classes
// each (PAD_BYTES of constant pool apiece, so a class is a few KB, as real
// ones are). Each round a FRESH `URLClassLoader` over the JARS `jar:nested:`
// URLs (the stand-in `Handler`, serving from memory) loads every class, so
// `Resource.getBytes()` runs once per class per round; round 0 also builds
// the per-VM bytes rows (one per nested jar), later rounds only read entries.
//
// Run (the outer index is on by default):
//   javac -d out L5W68NestedJarBytesDirectoryBench.java
//   cratonvm --java-home <jdk25> -cp out L5W68NestedJarBytesDirectoryBench                                                                         # bytes index off (the body)
//   CRATONVM_NESTED_JAR_BYTES_INDEX=1 cratonvm --java-home <jdk25> -cp out L5W68NestedJarBytesDirectoryBench                                       # owned copies
//   CRATONVM_NESTED_JAR_BYTES_INDEX=1 CRATONVM_NESTED_JAR_BYTES_DIRECTORY=1 cratonvm --java-home <jdk25> -cp out L5W68NestedJarBytesDirectoryBench  # directory rows
//   java -cp out L5W68NestedJarBytesDirectoryBench                                                                                                  # HotSpot
// Interleave the binaries and take the medians. For RSS run each row under
// `/usr/bin/time -v` (max resident set): the directory rows should sit near
// the bytes-index-off row, the owned copies above it by about the nested
// jars' total size (JARS * CLASSES * ~PAD_BYTES compressed). For the
// per-class system calls, `strace -f -c -e trace=openat,pread64,read,lseek`:
// the directory rows add one open + one or two reads per class.
//
// Prints `round=<k> ns/class=<n>` rows, then `first-round`, `later-rounds min`
// and `later-rounds median` to stderr; stdout is `loaded=<n> per round`, the
// same on every VM. Positive control: `CRATONVM_DBG=access` reads
// `nested-jar-bytes-served=` about `ROUNDS * JARS * CLASSES` with the bytes
// index on (either row kind), 0 with it off.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// loaded=1600 per round
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
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W68NestedJarBytesDirectoryBench {
    static final int JARS = 40;
    static final int CLASSES = 40;
    static final int PAD_BYTES = 3000;
    static final int ROUNDS = 7;

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
        out.writeByte(1); out.writeUTF("L5W68NestedJarBytesDirectoryBench");          // #11
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

    /** `public class <name> extends Object {}` with an unused PAD_BYTES-long UTF-8 constant. */
    static byte[] paddedClass(String name) throws IOException {
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
        out.writeByte(1); out.writeUTF("x".repeat(PAD_BYTES) + name);
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

    public static void main(String[] args) throws Exception {
        String[] inners = new String[JARS];
        String[] classes = new String[JARS * CLASSES];
        ByteArrayOutputStream fat = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(fat)) {
            for (int j = 0; j < JARS; j++) {
                Map<String, byte[]> entries = new LinkedHashMap<>();
                String pkg = "dep" + j;
                entries.put(pkg + "/", new byte[0]);
                for (int c = 0; c < CLASSES; c++) {
                    entries.put(pkg + "/K" + c + ".class", paddedClass(pkg + "/K" + c));
                    classes[j * CLASSES + c] = pkg + ".K" + c;
                }
                inners[j] = "BOOT-INF/lib/dep" + j + ".jar";
                CONTENT.put(inners[j], entries);
                // Deflated entries in a STORED nested jar: Spring Boot's layout.
                stored(zip, inners[j], jar(entries));
            }
        }
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W68NestedJarBytesDirectoryBench.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        Path dir = Files.createTempDirectory("l5w68-bytes-directory-bench");
        Path outer = dir.resolve("app.jar");
        long[] perClass = new long[ROUNDS];
        int loaded = 0;
        try {
            Files.write(outer, fat.toByteArray());
            String rawPath = outer.toUri().getRawPath().replace("!", "%21");
            URL[] urls = new URL[JARS];
            for (int j = 0; j < JARS; j++) {
                urls[j] = new URL(null, "jar:nested:" + rawPath + "/!" + inners[j] + "!/", handler);
            }
            for (int r = 0; r < ROUNDS; r++) {
                loaded = 0;
                // A fresh loader per round: every class is defined again, its
                // bytes read again (the per-VM rows stay: round 0 builds them).
                try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                    long t0 = System.nanoTime();
                    for (String name : classes) {
                        if (ucl.loadClass(name) != null) {
                            loaded++;
                        }
                    }
                    perClass[r] = (System.nanoTime() - t0) / classes.length;
                }
                System.err.println("round=" + r + " ns/class=" + perClass[r]);
            }
        } finally {
            Files.deleteIfExists(outer);
            Files.deleteIfExists(dir);
        }
        long[] later = Arrays.copyOfRange(perClass, 1, ROUNDS);
        Arrays.sort(later);
        System.err.println("first-round ns/class=" + perClass[0]);
        System.err.println("later-rounds min ns/class=" + later[0]);
        System.err.println("later-rounds median ns/class=" + later[later.length / 2]);
        System.out.println("loaded=" + loaded + " per round");
    }
}
