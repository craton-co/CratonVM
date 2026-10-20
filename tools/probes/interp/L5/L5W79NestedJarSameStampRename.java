// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 79, lane L5 -- a fat jar replaced through a
// rename by one of the SAME length and the SAME modification time
// (`docs/internal/fixed-bugs/interpreter-i68-L5-a-same-length-same-mtime-fat-jar-rewrite-keeps-a-stale-walk-row-FIXED-20261111.md`).
//
// The shape of `L5W79NestedJarSameStampRename` (a `URLClassLoader` over Spring
// Boot-shaped `jar:nested:` URLs whose stand-in handler reads the outer jar
// from disk on every open), but every entry is STORED and round 2's jar
// differs from round 1's only by the nested class `p/Aa` renamed `p/Ab` (same
// name length), so the two fat jars have the same length; round 2's is
// written to a temporary file, given round 1's modification time, and renamed
// over the path (as `cp -p` / `rsync -t` / a reproducible build copied over
// another would leave it). HotSpot runs every `URLClassPath$Loader` body: the
// second loader finds `p.Ab`. CratonVM's walk-index rows were stamped by
// (length, mtime) only, so the row built from round 1's jar kept answering
// "absent" for `p.Ab`. Since wave 79 the stamp carries the file identity
// (device + inode on Unix), so the rename is seen and the row rebuilt. On
// Windows the standard library has no stable file identity: the
// `class-Ab-2` line keeps the old answer there (not a target for this probe).
//
// Run (Linux; writes a temporary fat jar and deletes it):
//   javac -d out L5W79NestedJarSameStampRename.java
//   $J/java -cp out L5W79NestedJarSameStampRename                                # HotSpot
//   $B --java-home $J/.. [--nojit] [--compatible] -cp out L5W79NestedJarSameStampRename
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// same-length=true same-mtime=true
// class-Aa-1=loader=true source=b
// class-Ab-1=java.lang.ClassNotFoundException
// class-Ab-2=loader=true source=b
// class-Aa-2=java.lang.ClassNotFoundException
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

public class L5W79NestedJarSameStampRename {
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
        out.writeByte(1); out.writeUTF("L5W79NestedJarSameStampRename");               // #11
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

    static void writeFatJar(Path outer, String nestedClass) throws IOException {
        Map<String, byte[]> a = new LinkedHashMap<>();
        a.put("p/", new byte[0]);
        a.put("p/A.class", emptyClass("p/A"));
        Map<String, byte[]> b = new LinkedHashMap<>();
        b.put("p/", new byte[0]);
        b.put(nestedClass + ".class", emptyClass(nestedClass));
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

    static void ask(URLClassLoader ucl, String label, String name) {
        try {
            Class<?> c = ucl.loadClass(name);
            System.out.println(label + "=loader=" + (c.getClassLoader() == ucl) + " source=" + source(c));
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName());
        }
    }

    static void round(int n, URL[] urls) throws Exception {
        try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
            if (n == 1) {
                ask(ucl, "class-Aa-1", "p.Aa");
                ask(ucl, "class-Ab-1", "p.Ab");
            } else {
                ask(ucl, "class-Ab-2", "p.Ab");
                ask(ucl, "class-Aa-2", "p.Aa");
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w65-nested");
        Path outer = dir.resolve("outer.jar");
        OUTER = outer;
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W79NestedJarSameStampRename.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        String rawPath = outer.toUri().getRawPath().replace("!", "%21");
        URL[] urls = {
            new URL(null, "jar:nested:" + rawPath + "/!BOOT-INF/lib/a.jar!/", handler),
            new URL(null, "jar:nested:" + rawPath + "/!BOOT-INF/lib/b.jar!/", handler),
        };
        Path next = dir.resolve("outer.jar.next");
        try {
            writeFatJar(outer, "p/Aa");
            round(1, urls);
            long len = Files.size(outer);
            java.nio.file.attribute.FileTime mtime = Files.getLastModifiedTime(outer);
            writeFatJar(next, "p/Ab");
            Files.setLastModifiedTime(next, mtime);
            Files.move(next, outer, java.nio.file.StandardCopyOption.REPLACE_EXISTING,
                    java.nio.file.StandardCopyOption.ATOMIC_MOVE);
            System.out.println("same-length=" + (Files.size(outer) == len)
                    + " same-mtime=" + Files.getLastModifiedTime(outer).equals(mtime));
            round(2, urls);
        } finally {
            Files.deleteIfExists(next);
            Files.deleteIfExists(outer);
            Files.deleteIfExists(dir);
        }
    }
}
