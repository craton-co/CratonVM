// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 54, lane L5 -- the `URLClassPath$Loader` walk
// index (`CRATONVM_NESTED_JAR_WALK_INDEX`;
// `docs/internal/fixed-bugs/interpreter-L5-proposal-index-assisted-urlclasspath-walk-for-nested-jar-loaders-FIXED-20261018.md`).
// A `URLClassLoader` over Spring Boot-shaped `jar:nested:<outer>/!<inner>!/`
// URLs gets one generic `URLClassPath$Loader` per URL (the `jar:` handler is
// not the JDK's), and every lookup walks them in order, each step opening a
// connection. CratonVM answers a step whose nested jar's central directory
// lacks the name (and `name + "/"`) with the body's own `null`, without
// running it, when the handler is Spring's (by class name) -- the probe
// defines a stand-in handler under that name, with Spring's not-found rule.
//
// What must not change (and what the rows print): which loader a class
// comes from, its code source, the found loader still being asked (a PRESENT
// name's `opens`), resource contents and order, a directory entry found
// through `name + "/"`, absent names. The skipped steps are not printed (the
// stand-in handler counts them; Spring's real one is not observable).
// Rows first on a cold VM, again on a fresh loader.
//
// Run (no setup; writes a temporary fat jar and deletes it):
//   javac -d out L5W54NestedJarWalkIndex.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W54NestedJarWalkIndex
//
// Positive control: `CRATONVM_DBG=access` prints the LOADER census line;
// with the switch on (`CRATONVM_NESTED_JAR_WALK_INDEX`, and under
// `--compatible` the `jdk-url-class-loaders` row) `nested-jar-walk-skipped=`
// is non-zero (the B, absent and only-in-b rows skip `a.jar`); with
// `CRATONVM_NESTED_JAR_WALK_INDEX=0` it is 0 and the output is unchanged.
//
// The `class-V` / `res-versioned*` rows (wave-54 follow-up): `c.jar` is a
// Multi-Release jar whose `p.V` and `versioned.txt` exist ONLY under
// `META-INF/versions/9/`, served through the versioned view as Spring's
// `NestedJarFile` serves spring-core 7's `ClassFileMetadataReaderFactory`.
// The first index listed plain entry names only and skipped `c.jar`: measured
// on a dev build of `83816a7b3b`, both modes, `class-V-N=java.lang.ClassNotFoundException`,
// `res-versioned-N=null`, `res-versioned-all-N=` (the host's `--jdk-only`
// fat-jar `NoClassDefFoundError`).
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   class-V-1=loader=true source=c cOpens=1
//   res-versioned-1=v9
//   res-versioned-all-1=v9
//   class-A-1=loader=true source=a aOpens=1 bOpens=0
//   class-B-1=loader=true source=b bOpens=1
//   absent-1=java.lang.ClassNotFoundException
//   res-shared-1=a
//   res-all-1=a,b
//   res-only-b-1=b-only
//   res-dir-1=found=true
//   res-absent-1=null all=0
//   class-V-2=loader=true source=c cOpens=1
//   res-versioned-2=v9
//   res-versioned-all-2=v9
//   class-A-2=loader=true source=a aOpens=1 bOpens=0
//   class-B-2=loader=true source=b bOpens=1
//   absent-2=java.lang.ClassNotFoundException
//   res-shared-2=a
//   res-all-2=a,b
//   res-only-b-2=b-only
//   res-dir-2=found=true
//   res-absent-2=null all=0

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
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W54NestedJarWalkIndex {
    /** inner jar name -> entry name -> bytes (directories map to empty). */
    static final Map<String, Map<String, byte[]>> JARS = new HashMap<>();
    static final Map<String, AtomicLong> OPENS = new ConcurrentHashMap<>();

    /** The stand-in handler's `openConnection`: Spring's not-found rule. */
    public static URLConnection open(URL url) throws IOException {
        String file = url.getFile();
        int sep = file.indexOf("!/");
        String nested = file.substring(0, sep);
        String entry = file.substring(sep + 2);
        String inner = nested.substring(nested.indexOf("/!") + 2);
        OPENS.computeIfAbsent(inner, k -> new AtomicLong()).incrementAndGet();
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
        out.writeByte(1); out.writeUTF("L5W54NestedJarWalkIndex");                 // #11
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

    static long opens(String inner) {
        AtomicLong n = OPENS.get(inner);
        return n == null ? 0 : n.get();
    }

    static String text(URL url) throws IOException {
        if (url == null) {
            return "null";
        }
        try (InputStream in = url.openStream()) {
            return new String(in.readAllBytes(), "UTF-8");
        }
    }

    static String source(Class<?> c) {
        String loc = String.valueOf(c.getProtectionDomain().getCodeSource().getLocation());
        return loc.endsWith("/a.jar!/") ? "a" : loc.endsWith("/b.jar!/") ? "b"
                : loc.endsWith("/c.jar!/") ? "c" : "other";
    }

    static void round(int n, URL[] urls) throws Exception {
        try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
            // Multi-Release `c.jar`: `p.V` and `versioned.txt` exist ONLY under
            // `META-INF/versions/9/` (the wave-54 follow-up; spring-core 7's
            // `ClassFileMetadataReaderFactory` is such a class).
            OPENS.clear();
            try {
                Class<?> v = ucl.loadClass("p.V");
                System.out.println("class-V-" + n + "=loader=" + (v.getClassLoader() == ucl)
                        + " source=" + source(v) + " cOpens=" + opens("BOOT-INF/lib/c.jar"));
            } catch (Throwable t) {
                System.out.println("class-V-" + n + "=" + t.getClass().getName());
            }
            System.out.println("res-versioned-" + n + "=" + text(ucl.getResource("versioned.txt")));
            List<String> allVersioned = new ArrayList<>();
            for (URL u : Collections.list(ucl.getResources("versioned.txt"))) {
                allVersioned.add(text(u));
            }
            System.out.println("res-versioned-all-" + n + "=" + String.join(",", allVersioned));
            OPENS.clear();
            Class<?> a = ucl.loadClass("p.A");
            System.out.println("class-A-" + n + "=loader=" + (a.getClassLoader() == ucl) + " source="
                    + source(a) + " aOpens=" + opens("BOOT-INF/lib/a.jar") + " bOpens="
                    + opens("BOOT-INF/lib/b.jar"));
            OPENS.clear();
            Class<?> b = ucl.loadClass("p.B");
            System.out.println("class-B-" + n + "=loader=" + (b.getClassLoader() == ucl) + " source="
                    + source(b) + " bOpens=" + opens("BOOT-INF/lib/b.jar"));
            try {
                ucl.loadClass("p.Missing");
                System.out.println("absent-" + n + "=found");
            } catch (Throwable t) {
                System.out.println("absent-" + n + "=" + t.getClass().getName());
            }
            System.out.println("res-shared-" + n + "=" + text(ucl.getResource("shared.txt")));
            List<String> all = new ArrayList<>();
            for (URL u : Collections.list(ucl.getResources("shared.txt"))) {
                all.add(text(u));
            }
            System.out.println("res-all-" + n + "=" + String.join(",", all));
            System.out.println("res-only-b-" + n + "=" + text(ucl.getResource("only-b.txt")));
            System.out.println("res-dir-" + n + "=found=" + (ucl.getResource("dir") != null));
            Enumeration<URL> none = ucl.getResources("nope.txt");
            System.out.println("res-absent-" + n + "=" + ucl.getResource("nope.txt") + " all="
                    + Collections.list(none).size());
        }
    }

    public static void main(String[] args) throws Exception {
        Map<String, byte[]> a = new LinkedHashMap<>();
        a.put("p/", new byte[0]);
        a.put("p/A.class", emptyClass("p/A"));
        a.put("shared.txt", "a".getBytes("UTF-8"));
        a.put("dir/", new byte[0]);
        a.put("dir/x.txt", "x".getBytes("UTF-8"));
        Map<String, byte[]> b = new LinkedHashMap<>();
        b.put("p/", new byte[0]);
        b.put("p/B.class", emptyClass("p/B"));
        b.put("shared.txt", "b".getBytes("UTF-8"));
        b.put("only-b.txt", "b-only".getBytes("UTF-8"));
        Map<String, byte[]> c = new LinkedHashMap<>();
        c.put("META-INF/", new byte[0]);
        c.put("META-INF/MANIFEST.MF",
                "Manifest-Version: 1.0\r\nMulti-Release: true\r\n\r\n".getBytes("UTF-8"));
        c.put("META-INF/versions/", new byte[0]);
        c.put("META-INF/versions/9/", new byte[0]);
        c.put("META-INF/versions/9/p/", new byte[0]);
        c.put("META-INF/versions/9/p/V.class", emptyClass("p/V"));
        c.put("META-INF/versions/9/versioned.txt", "v9".getBytes("UTF-8"));
        JARS.put("BOOT-INF/lib/a.jar", a);
        JARS.put("BOOT-INF/lib/b.jar", b);
        JARS.put("BOOT-INF/lib/c.jar", c);

        Path dir = Files.createTempDirectory("l5w54-nested");
        Path outer = dir.resolve("outer.jar");
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(outer))) {
            stored(zip, "BOOT-INF/lib/a.jar", jar(a));
            stored(zip, "BOOT-INF/lib/b.jar", jar(b));
            stored(zip, "BOOT-INF/lib/c.jar", jar(c));
        }
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W54NestedJarWalkIndex.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        String rawPath = outer.toUri().getRawPath().replace("!", "%21");
        URL[] urls = {
            new URL(null, "jar:nested:" + rawPath + "/!BOOT-INF/lib/a.jar!/", handler),
            new URL(null, "jar:nested:" + rawPath + "/!BOOT-INF/lib/b.jar!/", handler),
            new URL(null, "jar:nested:" + rawPath + "/!BOOT-INF/lib/c.jar!/", handler),
        };
        try {
            round(1, urls);
            round(2, urls);
        } finally {
            Files.deleteIfExists(outer);
            Files.deleteIfExists(dir);
        }
    }
}
