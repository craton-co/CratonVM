// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 69, lane L5 -- timing for
// `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER` (default off): nested-jar walk and
// bytes rows for a fat jar that is ON THE VM'S APPLICATION CLASS PATH (the
// `java -jar app.jar` shape) built from the class path's own bytes and parse
// -- no open, no positioned read of the nested jar, no parse of the outer
// directory -- instead of through the outer index (default on), which opens
// the fat jar and reads one nested jar per row build
// (`docs/internal/proposals/interpreter/i66-L5-proposal-reuse-the-class-path-parse-of-a-fat-jar-20261030.md`).
//
// The fat jar must be on `-cp` when the VM starts, so: `--make <dir>/app.jar`
// writes it (any JVM, deterministic class bytes), then the bench runs with it
// on the class path. JARS stored nested jars (outside `BOOT-INF/`, so no VM
// mode emulates a fat jar on the class path) of CLASSES deflated classes.
// Round 0 builds every row (one walk row and one bytes row per nested jar):
// its `ns/class` is where the switch shows; later rounds only read entries
// (from the class path's mapping with the switch: no copy of the nested jar
// either, so the RSS of the row-holding bytes index drops too).
//
// Run (interleave, take medians; Linux/macOS):
//   javac -d out L5W69NestedJarClassPathOuterBench.java
//   java -cp out L5W69NestedJarClassPathOuterBench --make /tmp/l5w69cpb/app.jar
//   CRATONVM_NESTED_JAR_BYTES_INDEX=1 CRATONVM_NESTED_JAR_CLASS_PATH_OUTER=1 cratonvm --java-home <jdk25> -cp out:/tmp/l5w69cpb/app.jar L5W69NestedJarClassPathOuterBench
//   CRATONVM_NESTED_JAR_BYTES_INDEX=1 cratonvm --java-home <jdk25> -cp out:/tmp/l5w69cpb/app.jar L5W69NestedJarClassPathOuterBench   # outer index
//   CRATONVM_NESTED_JAR_CLASS_PATH_OUTER=1 cratonvm --java-home <jdk25> -cp out:/tmp/l5w69cpb/app.jar L5W69NestedJarClassPathOuterBench  # walk rows only
//   java -cp out:/tmp/l5w69cpb/app.jar L5W69NestedJarClassPathOuterBench                                                               # HotSpot
// System calls: `strace -f -c -e trace=openat,pread64,read,lseek,newfstatat`:
// with the switch the per-row `openat` + reads of app.jar disappear (one
// `stat` per row build remains: the stamp check). RSS: `/usr/bin/time -v`.
//
// Prints `round=<k> ns/class=<n>`, `first-round`, `later-rounds min` and
// `later-rounds median` to stderr; stdout is `on-class-path=true` and
// `loaded=<n> per round`, the same on every VM.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// on-class-path=true
// loaded=2000 per round
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.URLConnection;
import java.net.URLStreamHandler;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Arrays;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W69NestedJarClassPathOuterBench {
    static final int JARS = 200;
    static final int CLASSES = 10;
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
        out.writeByte(1); out.writeUTF("L5W69NestedJarClassPathOuterBench");          // #11
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
                inners[j] = "lib/dep" + j + ".jar";
                CONTENT.put(inners[j], entries);
                // Deflated entries in a STORED nested jar: Spring Boot's layout
                // (outside BOOT-INF/: see the header).
                stored(zip, inners[j], jar(entries));
            }
        }
        if (args.length == 2 && args[0].equals("--make")) {
            Path jar = Paths.get(args[1]).toAbsolutePath();
            Files.createDirectories(jar.getParent());
            Files.write(jar, fat.toByteArray());
            System.out.println("made " + jar);
            return;
        }
        Path outer = null;
        for (String element : System.getProperty("java.class.path").split(File.pathSeparator)) {
            if (element.endsWith("app.jar")) {
                outer = Paths.get(element).toAbsolutePath();
            }
        }
        System.out.println("on-class-path=" + (outer != null && Files.isRegularFile(outer)));
        if (outer == null) {
            return;
        }
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W69NestedJarClassPathOuterBench.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        long[] perClass = new long[ROUNDS];
        int loaded = 0;
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
        long[] later = Arrays.copyOfRange(perClass, 1, ROUNDS);
        Arrays.sort(later);
        System.err.println("first-round ns/class=" + perClass[0]);
        System.err.println("later-rounds min ns/class=" + later[0]);
        System.err.println("later-rounds median ns/class=" + later[later.length / 2]);
        System.out.println("loaded=" + loaded + " per round");
    }
}
