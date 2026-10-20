// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 63, lane L5 -- timing for the nested-jar bytes
// index (`CRATONVM_NESTED_JAR_BYTES_INDEX`; see `L5W63NestedJarBytesIndex`
// for the behaviour). A `URLClassLoader` over one Spring Boot-shaped
// `jar:nested:<outer>/!BOOT-INF/lib/bench.jar!/` URL, with a stand-in handler
// under Spring's class name whose connection reads the class through a real
// `java.util.jar.JarFile` over the nested jar (an `InflaterInputStream` for
// the DEFLATED entries, as Spring's `NestedJarFile` hands one out), defines
// CLASSES classes of about 4 KB each per round on a fresh loader. The row is
// the time per defined class: the JDK's `Resource.getBytes` read loop over
// that stream (switch off) against the VM's own copy of the entry (switch on).
//
// Run:
//   javac -d out L5W63NestedJarBytesBench.java
//   cratonvm --java-home <jdk25> -cp out L5W63NestedJarBytesBench                      # control
//   CRATONVM_NESTED_JAR_BYTES_INDEX=1 cratonvm --java-home <jdk25> -cp out L5W63NestedJarBytesBench
//   java -cp out L5W63NestedJarBytesBench                                               # HotSpot
// Interleave the binaries and take medians (in-VM timings swing between
// repetitions on the dev hosts). The last line, `loaded=<CLASSES> per round`,
// is the same on every VM.
//
// Prints one `round=<k> ns/op=<n>` row per round, then `median ns/op=<n>`.

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
import java.util.jar.JarFile;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W63NestedJarBytesBench {
    static final int CLASSES = 300;
    static final int ROUNDS = 9;
    static volatile JarFile nested;

    /** The stand-in handler's `openConnection`. */
    public static URLConnection open(URL url) throws IOException {
        String file = url.getFile();
        String entry = file.substring(file.indexOf("!/") + 2);
        return new Conn(url, entry);
    }

    static final class Conn extends URLConnection {
        final String entry;

        Conn(URL url, String entry) {
            super(url);
            this.entry = entry;
        }

        @Override
        public void connect() {
        }

        ZipEntry zipEntry() throws IOException {
            ZipEntry e = nested.getEntry(entry);
            if (e == null) {
                throw new FileNotFoundException("Jar file or entry not found");
            }
            return e;
        }

        @Override
        public InputStream getInputStream() throws IOException {
            return nested.getInputStream(zipEntry());
        }

        @Override
        public int getContentLength() {
            try {
                return (int) zipEntry().getSize();
            } catch (IOException e) {
                return -1;
            }
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

    /** Spring Boot's handler's class name, `openConnection` calling `<owner>.open`. */
    static byte[] handlerClass(String owner) throws IOException {
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
        out.writeByte(1); out.writeUTF(owner);                                     // #11
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
        ByteArrayOutputStream jarBytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(jarBytes)) {
            zip.putNextEntry(new ZipEntry("p/"));
            zip.closeEntry();
            for (int i = 0; i < CLASSES; i++) {
                zip.putNextEntry(new ZipEntry("p/C" + i + ".class"));
                zip.write(paddedClass("p/C" + i, 4000));
                zip.closeEntry();
            }
        }
        Path dir = Files.createTempDirectory("l5w63-bench");
        Path outer = dir.resolve("outer.jar");
        Path inner = dir.resolve("bench.jar");
        Files.write(inner, jarBytes.toByteArray());
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(outer))) {
            stored(zip, "BOOT-INF/lib/bench.jar", jarBytes.toByteArray());
        }
        nested = new JarFile(inner.toFile());
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W63NestedJarBytesBench.class.getClassLoader())
                .define(handlerClass("L5W63NestedJarBytesBench"))
                .getDeclaredConstructor().newInstance();
        String rawPath = outer.toUri().getRawPath().replace("!", "%21");
        URL[] urls = {new URL(null, "jar:nested:" + rawPath + "/!BOOT-INF/lib/bench.jar!/", handler)};
        long[] perClass = new long[ROUNDS];
        int loaded = 0;
        try {
            for (int r = 0; r < ROUNDS; r++) {
                loaded = 0;
                try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                    long t0 = System.nanoTime();
                    for (int i = 0; i < CLASSES; i++) {
                        if (ucl.loadClass("p.C" + i).getClassLoader() == ucl) {
                            loaded++;
                        }
                    }
                    perClass[r] = (System.nanoTime() - t0) / CLASSES;
                }
                System.out.println("round=" + r + " ns/op=" + perClass[r]);
            }
        } finally {
            nested.close();
            Files.deleteIfExists(outer);
            Files.deleteIfExists(inner);
            Files.deleteIfExists(dir);
        }
        long[] sorted = perClass.clone();
        Arrays.sort(sorted);
        System.out.println("median ns/op=" + sorted[ROUNDS / 2]);
        System.out.println("loaded=" + loaded + " per round");
    }
}
