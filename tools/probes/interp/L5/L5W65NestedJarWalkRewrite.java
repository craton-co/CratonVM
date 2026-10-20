// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 65, lane L5 -- the nested-jar walk index moved
// per VM, with the stamped rows of `CRATONVM_NESTED_JAR_WALK_INDEX_STAMP`
// (default off;
// `docs/internal/fixed-bugs/interpreter-i64-L5-proposal-move-the-nested-jar-walk-index-per-vm-DONE-20261028.md`,
// `docs/internal/fixed-bugs/interpreter-i63-L5-the-nested-jar-walk-index-never-notices-a-rewritten-fat-jar-FIXED-20261027.md`).
//
// A `URLClassLoader` over Spring Boot-shaped `jar:nested:<outer>/!<inner>!/`
// URLs (the stand-in `Handler` of `L5W54NestedJarWalkIndex`, which here
// reads the outer jar FROM DISK on every open, as Spring's handler does once
// its cached `ZipContent` is closed) loads from `a.jar` and misses `p.New`.
// The loader is closed, the fat jar is rewritten at the same path with
// `p.New` and `new.txt` added to `b.jar`, and a fresh loader asks again.
// HotSpot runs every `URLClassPath$Loader` body: the second loader finds
// both. CratonVM's walk index answers `null` for a nested jar whose index
// lacks the name; a row built from the old jar keeps saying "absent".
//
// Run (no setup; writes a temporary fat jar and deletes it):
//   javac -d out L5W65NestedJarWalkRewrite.java
//   CRATONVM_NESTED_JAR_WALK_INDEX_STAMP=1 cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W65NestedJarWalkRewrite
//
// Expected on CratonVM WITHOUT the stamp (the i63 defect, reproduced by
// code reading; the walk index is default on): `class-New-2=java.lang.ClassNotFoundException`,
// `res-new-2=null`, `res-new-all-2=` (the other rows as below). With
// `CRATONVM_NESTED_JAR_WALK_INDEX_STAMP=1` the output is HotSpot's. With
// `CRATONVM_NESTED_JAR_WALK_INDEX=0` it is HotSpot's too (every body runs).
// Positive control: `CRATONVM_DBG=access` LOADER census
// `nested-jar-walk-skipped=` stays non-zero with the stamp on (round 1's
// `p.New` / `new.txt` asks skip both jars, round 2's `p.Missing` too): the
// stamp rebuilds the rewritten jar's rows, it does not turn the index off.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// class-A-1=loader=true source=a
// class-New-1=java.lang.ClassNotFoundException
// res-new-1=null
// res-new-all-1=
// class-A-2=loader=true source=a
// class-New-2=loader=true source=b
// res-new-2=new
// res-new-all-2=new
// absent-2=java.lang.ClassNotFoundException
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
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipInputStream;
import java.util.zip.ZipOutputStream;

public class L5W65NestedJarWalkRewrite {
    static volatile Path OUTER;

    /** The stand-in handler's `openConnection`: Spring's not-found rule, read from disk. */
    public static URLConnection open(URL url) throws IOException {
        String file = url.getFile();
        int sep = file.indexOf("!/");
        String nested = file.substring(0, sep);
        String entry = file.substring(sep + 2);
        String inner = nested.substring(nested.indexOf("/!") + 2);
        return new Conn(url, read(inner, entry));
    }

    /** `entry` (or `entry + "/"`) of the nested jar `inner` in the outer jar as it is on disk now. */
    static byte[] read(String inner, String entry) throws IOException {
        byte[] nestedBytes;
        try (ZipFile outer = new ZipFile(OUTER.toFile())) {
            ZipEntry e = outer.getEntry(inner);
            if (e == null) {
                return null;
            }
            try (InputStream in = outer.getInputStream(e)) {
                nestedBytes = in.readAllBytes();
            }
        }
        try (ZipInputStream zin = new ZipInputStream(new ByteArrayInputStream(nestedBytes))) {
            for (ZipEntry e; (e = zin.getNextEntry()) != null; ) {
                if (e.getName().equals(entry) || e.getName().equals(entry + "/")) {
                    return zin.readAllBytes();
                }
            }
        }
        return null;
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
        out.writeByte(1); out.writeUTF("L5W65NestedJarWalkRewrite");               // #11
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

    static void writeFatJar(Path outer, boolean withNew) throws IOException {
        Map<String, byte[]> a = new LinkedHashMap<>();
        a.put("p/", new byte[0]);
        a.put("p/A.class", emptyClass("p/A"));
        Map<String, byte[]> b = new LinkedHashMap<>();
        b.put("q/", new byte[0]);
        b.put("q/B.class", emptyClass("q/B"));
        if (withNew) {
            b.put("p/", new byte[0]);
            b.put("p/New.class", emptyClass("p/New"));
            b.put("new.txt", "new".getBytes("UTF-8"));
        }
        // Rewritten in place (same path), as a build tool rewrites its output.
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(outer))) {
            stored(zip, "BOOT-INF/lib/a.jar", jar(a));
            stored(zip, "BOOT-INF/lib/b.jar", jar(b));
        }
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
        return loc.endsWith("/a.jar!/") ? "a" : loc.endsWith("/b.jar!/") ? "b" : "other";
    }

    static void round(int n, URL[] urls) throws Exception {
        try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
            Class<?> a = ucl.loadClass("p.A");
            System.out.println("class-A-" + n + "=loader=" + (a.getClassLoader() == ucl)
                    + " source=" + source(a));
            try {
                Class<?> c = ucl.loadClass("p.New");
                System.out.println("class-New-" + n + "=loader=" + (c.getClassLoader() == ucl)
                        + " source=" + source(c));
            } catch (Throwable t) {
                System.out.println("class-New-" + n + "=" + t.getClass().getName());
            }
            System.out.println("res-new-" + n + "=" + text(ucl.getResource("new.txt")));
            List<String> all = new ArrayList<>();
            for (URL u : Collections.list(ucl.getResources("new.txt"))) {
                all.add(text(u));
            }
            System.out.println("res-new-all-" + n + "=" + String.join(",", all));
            if (n == 2) {
                try {
                    ucl.loadClass("p.Missing");
                    System.out.println("absent-" + n + "=found");
                } catch (Throwable t) {
                    System.out.println("absent-" + n + "=" + t.getClass().getName());
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w65-nested");
        Path outer = dir.resolve("outer.jar");
        OUTER = outer;
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W65NestedJarWalkRewrite.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        String rawPath = outer.toUri().getRawPath().replace("!", "%21");
        URL[] urls = {
            new URL(null, "jar:nested:" + rawPath + "/!BOOT-INF/lib/a.jar!/", handler),
            new URL(null, "jar:nested:" + rawPath + "/!BOOT-INF/lib/b.jar!/", handler),
        };
        try {
            writeFatJar(outer, false);
            round(1, urls);
            writeFatJar(outer, true);
            round(2, urls);
        } finally {
            Files.deleteIfExists(outer);
            Files.deleteIfExists(dir);
        }
    }
}
