// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 63, lane L5 -- the nested-jar bytes index
// (`CRATONVM_NESTED_JAR_BYTES_INDEX`, default off;
// `docs/internal/fixed-bugs/interpreter-i54-L5-proposal-serve-a-nested-jar-hits-class-bytes-from-the-vm-index-KEPT-OFF-20261019.md`).
// A `URLClassLoader` over Spring Boot-shaped `jar:nested:<outer>/!<inner>!/`
// URLs gets one generic `URLClassPath$Loader` per URL, and defining a class
// it found reads the class file through `Resource.getBytes()` -- the JDK's
// read loop over the connection's stream (Spring's `NestedJarFile` inflater
// stack on a real fat jar). With the switch on, CratonVM answers that call
// from its own copy of the nested jar when the jar is plain (not
// Multi-Release, unsigned) and the URL's handler is Spring's (by class name:
// the probe defines a stand-in handler under that name, as
// `L5W54NestedJarWalkIndex` does).
//
// What must not change (and what the rows print): which loader a class comes
// from, its code source and signers, the defined package, the calls the JDK
// body makes on the connection and its stream (`streams` = `getInputStream`
// calls, 2 per class: `Loader.getResource` and `cachedInputStream`; `lens` =
// `getContentLength` calls; `closes` = the cached stream's `close`), and the
// content-length rule of `getBytes`: a short length truncates the class file
// (`ClassFormatError`), a long one is `EOFException` wrapped in
// `ClassNotFoundException`, `-1` reads to the end. The reads themselves are
// not printed (the served path skips them; they are the cost). `a.jar` holds
// DEFLATED entries, `b.jar` and `l.jar` STORED ones; `s.jar` (a signature
// file) and `c.jar` (Multi-Release, `p.V` only under `META-INF/versions/9/`)
// are controls the intrinsic leaves to the body. Rows on a cold VM, then
// again on a fresh loader.
//
// Run (no setup; writes a temporary fat jar and deletes it):
//   javac -d out L5W63NestedJarBytesIndex.java
//   CRATONVM_NESTED_JAR_BYTES_INDEX=1 cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W63NestedJarBytesIndex
//
// Positive control: with `CRATONVM_DBG=access` the LOADER census line reads
// `nested-jar-bytes-served=10 nested-jar-bytes-body=4` with the switch on
// (A, B, Unknown, Short, Long served per round; S and V run the body), and
// `0` / `0` with it off (not registered); the output is the same either way.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// class-A-1=loader=true source=a streams=2 lens=1 closes=1 signers=null
// class-B-1=loader=true source=b streams=2 lens=1 closes=1 signers=null
// class-Unknown-1=loader=true source=l streams=2 lens=1 closes=1 signers=null
// class-S-1=loader=true source=s streams=2 lens=1 closes=1 signers=null
// class-V-1=loader=true source=c streams=2 lens=1 closes=1 signers=null
// short-1=java.lang.ClassFormatError streams=2 lens=1 closes=1
// long-1=java.lang.ClassNotFoundException cause=java.io.EOFException: Detect premature EOF streams=2 lens=1 closes=1
// pkg-1=p title=null vendor=null sealed=false
// res-A-1=3000
// class-A-2=loader=true source=a streams=2 lens=1 closes=1 signers=null
// class-B-2=loader=true source=b streams=2 lens=1 closes=1 signers=null
// class-Unknown-2=loader=true source=l streams=2 lens=1 closes=1 signers=null
// class-S-2=loader=true source=s streams=2 lens=1 closes=1 signers=null
// class-V-2=loader=true source=c streams=2 lens=1 closes=1 signers=null
// short-2=java.lang.ClassFormatError streams=2 lens=1 closes=1
// long-2=java.lang.ClassNotFoundException cause=java.io.EOFException: Detect premature EOF streams=2 lens=1 closes=1
// pkg-2=p title=null vendor=null sealed=false
// res-A-2=3000
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
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W63NestedJarBytesIndex {
    /** inner jar name -> entry name -> bytes (directories map to empty). */
    static final Map<String, Map<String, byte[]>> JARS = new HashMap<>();
    static final Map<String, AtomicLong> COUNTS = new ConcurrentHashMap<>();

    static void count(String what) {
        COUNTS.computeIfAbsent(what, k -> new AtomicLong()).incrementAndGet();
    }

    static long counted(String what) {
        AtomicLong n = COUNTS.get(what);
        return n == null ? 0 : n.get();
    }

    /** The stand-in handler's `openConnection`: Spring's not-found rule. */
    public static URLConnection open(URL url) throws IOException {
        String file = url.getFile();
        int sep = file.indexOf("!/");
        String nested = file.substring(0, sep);
        String entry = file.substring(sep + 2);
        String inner = nested.substring(nested.indexOf("/!") + 2);
        Map<String, byte[]> jar = JARS.get(inner);
        byte[] bytes = jar == null ? null : jar.get(entry);
        if (bytes == null && jar != null) {
            bytes = jar.get(entry + "/");
        }
        // A Multi-Release jar's versioned view (Spring opens a nested jar at
        // the runtime version): `META-INF/versions/9/<entry>` serves `<entry>`.
        if (bytes == null && jar != null && jar.containsKey("META-INF/versions/")) {
            bytes = jar.get("META-INF/versions/9/" + entry);
        }
        return new Conn(url, inner, entry, bytes);
    }

    static final class Conn extends URLConnection {
        final String inner;
        final String entry;
        final byte[] bytes;

        Conn(URL url, String inner, String entry, byte[] bytes) {
            super(url);
            this.inner = inner;
            this.entry = entry;
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
            count(inner + ":streams");
            return new ByteArrayInputStream(bytes) {
                @Override
                public void close() throws IOException {
                    count(inner + ":closes");
                    super.close();
                }
            };
        }

        @Override
        public int getContentLength() {
            if (bytes == null) {
                return -1;
            }
            count(inner + ":lens");
            if (entry.endsWith("/Short.class")) {
                return bytes.length - 10;
            }
            if (entry.endsWith("/Long.class")) {
                return bytes.length + 10;
            }
            if (entry.endsWith("/Unknown.class")) {
                return -1;
            }
            return bytes.length;
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

    /**
     * `public class <name> extends Object {}` (version 49), padded with an
     * unused `CONSTANT_Utf8` so the class file is exactly `size` bytes (the
     * body's read loop then takes several 1024-byte steps).
     */
    static byte[] paddedClass(String name, int size) throws IOException {
        // header 10, #1 3+n, #2 3, #3 3+16, #4 3, #5's header 3, the 7 u2 tail 14.
        int fixed = 10 + (3 + name.length()) + 3 + (3 + "java/lang/Object".length()) + 3 + 3 + 14;
        int pad = size - fixed;
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < pad; i++) {
            text.append((char) ('a' + i % 26));
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
        byte[] b = bytes.toByteArray();
        if (b.length != size) {
            throw new IllegalStateException("padded class is " + b.length + " bytes, not " + size);
        }
        return b;
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
        // public <init>()V { aload_0; invokespecial URLStreamHandler.<init>; return }
        out.writeShort(0x01); out.writeShort(5); out.writeShort(6); out.writeShort(1);
        out.writeShort(16); out.writeInt(17);
        out.writeShort(1); out.writeShort(1); out.writeInt(5);
        out.writeByte(0x2A); out.writeByte(0xB7); out.writeShort(8); out.writeByte(0xB1);
        out.writeShort(0); out.writeShort(0);
        // public openConnection(URL) { aload_1; invokestatic open; areturn }
        out.writeShort(0x01); out.writeShort(9); out.writeShort(10); out.writeShort(1);
        out.writeShort(16); out.writeInt(17);
        out.writeShort(1); out.writeShort(2); out.writeInt(5);
        out.writeByte(0x2B); out.writeByte(0xB8); out.writeShort(15); out.writeByte(0xB0);
        out.writeShort(0); out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    /** A jar whose entries are DEFLATED (`ZipOutputStream`'s default). */
    static byte[] deflatedJar(Map<String, byte[]> entries) throws IOException {
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

    /** A jar whose entries are STORED. */
    static byte[] storedJar(Map<String, byte[]> entries) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            for (Map.Entry<String, byte[]> e : entries.entrySet()) {
                stored(zip, e.getKey(), e.getValue());
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

    static String source(Class<?> c) {
        String loc = String.valueOf(c.getProtectionDomain().getCodeSource().getLocation());
        for (String jar : new String[] {"a", "b", "c", "l", "s"}) {
            if (loc.endsWith("/" + jar + ".jar!/")) {
                return jar;
            }
        }
        return "other";
    }

    static String calls(String jar) {
        String inner = "BOOT-INF/lib/" + jar + ".jar";
        return "streams=" + counted(inner + ":streams") + " lens=" + counted(inner + ":lens")
                + " closes=" + counted(inner + ":closes");
    }

    static void classRow(URLClassLoader ucl, int n, String simple, String jar) {
        COUNTS.clear();
        try {
            Class<?> c = ucl.loadClass("p." + simple);
            Object signers = c.getProtectionDomain().getCodeSource().getCodeSigners();
            System.out.println("class-" + simple + "-" + n + "=loader=" + (c.getClassLoader() == ucl)
                    + " source=" + source(c) + " " + calls(jar) + " signers=" + signers);
        } catch (Throwable t) {
            System.out.println("class-" + simple + "-" + n + "=" + t + " " + calls(jar));
        }
    }

    static void errorRow(URLClassLoader ucl, int n, String row, String simple) {
        COUNTS.clear();
        try {
            ucl.loadClass("p." + simple);
            System.out.println(row + "-" + n + "=loaded " + calls("l"));
        } catch (Throwable t) {
            Throwable cause = t.getCause();
            System.out.println(row + "-" + n + "=" + t.getClass().getName()
                    + (cause == null ? "" : " cause=" + cause) + " " + calls("l"));
        }
    }

    static void round(int n, URL[] urls) throws Exception {
        try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
            classRow(ucl, n, "A", "a");
            classRow(ucl, n, "B", "b");
            classRow(ucl, n, "Unknown", "l");
            classRow(ucl, n, "S", "s");
            classRow(ucl, n, "V", "c");
            errorRow(ucl, n, "short", "Short");
            errorRow(ucl, n, "long", "Long");
            Package p = ucl.loadClass("p.A").getPackage();
            System.out.println("pkg-" + n + "=" + p.getName() + " title=" + p.getImplementationTitle()
                    + " vendor=" + p.getImplementationVendor() + " sealed=" + p.isSealed());
            try (InputStream in = ucl.getResourceAsStream("p/A.class")) {
                System.out.println("res-A-" + n + "=" + (in == null ? "null" : in.readAllBytes().length));
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Map<String, byte[]> a = new LinkedHashMap<>();
        a.put("p/", new byte[0]);
        a.put("p/A.class", paddedClass("p/A", 3000));
        Map<String, byte[]> b = new LinkedHashMap<>();
        b.put("p/", new byte[0]);
        b.put("p/B.class", paddedClass("p/B", 2500));
        Map<String, byte[]> l = new LinkedHashMap<>();
        l.put("p/", new byte[0]);
        l.put("p/Unknown.class", paddedClass("p/Unknown", 2100));
        l.put("p/Short.class", paddedClass("p/Short", 2200));
        l.put("p/Long.class", paddedClass("p/Long", 2300));
        Map<String, byte[]> s = new LinkedHashMap<>();
        s.put("META-INF/", new byte[0]);
        s.put("META-INF/SIGNER.SF", "Signature-Version: 1.0\r\n\r\n".getBytes("UTF-8"));
        s.put("p/", new byte[0]);
        s.put("p/S.class", paddedClass("p/S", 1500));
        Map<String, byte[]> c = new LinkedHashMap<>();
        c.put("META-INF/", new byte[0]);
        c.put("META-INF/MANIFEST.MF",
                "Manifest-Version: 1.0\r\nMulti-Release: true\r\n\r\n".getBytes("UTF-8"));
        c.put("META-INF/versions/", new byte[0]);
        c.put("META-INF/versions/9/", new byte[0]);
        c.put("META-INF/versions/9/p/", new byte[0]);
        c.put("META-INF/versions/9/p/V.class", paddedClass("p/V", 1200));
        JARS.put("BOOT-INF/lib/a.jar", a);
        JARS.put("BOOT-INF/lib/b.jar", b);
        JARS.put("BOOT-INF/lib/l.jar", l);
        JARS.put("BOOT-INF/lib/s.jar", s);
        JARS.put("BOOT-INF/lib/c.jar", c);

        Path dir = Files.createTempDirectory("l5w63-nested");
        Path outer = dir.resolve("outer.jar");
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(outer))) {
            stored(zip, "BOOT-INF/lib/a.jar", deflatedJar(a));
            stored(zip, "BOOT-INF/lib/b.jar", storedJar(b));
            stored(zip, "BOOT-INF/lib/l.jar", storedJar(l));
            stored(zip, "BOOT-INF/lib/s.jar", deflatedJar(s));
            stored(zip, "BOOT-INF/lib/c.jar", deflatedJar(c));
        }
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W63NestedJarBytesIndex.class.getClassLoader())
                .define(handlerClass("L5W63NestedJarBytesIndex"))
                .getDeclaredConstructor().newInstance();
        String rawPath = outer.toUri().getRawPath().replace("!", "%21");
        URL[] urls = new URL[5];
        String[] order = {"a", "b", "l", "s", "c"};
        for (int i = 0; i < order.length; i++) {
            urls[i] = new URL(null, "jar:nested:" + rawPath + "/!BOOT-INF/lib/" + order[i] + ".jar!/", handler);
        }
        try {
            round(1, urls);
            round(2, urls);
        } finally {
            Files.deleteIfExists(outer);
            Files.deleteIfExists(dir);
        }
    }
}
