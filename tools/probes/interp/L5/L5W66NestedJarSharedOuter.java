// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 66, lane L5 -- one open of a fat jar's outer
// archive per VM for the nested-jar walk and bytes indexes
// (`CRATONVM_NESTED_JAR_SHARED_OUTER`, default off;
// `docs/internal/fixed-bugs/interpreter-i65-L5-proposal-one-outer-archive-per-fat-jar-per-vm-DONE-20261029.md`).
//
// A `URLClassLoader` over Spring Boot-shaped `jar:nested:<outer>/!<inner>!/`
// URLs (the stand-in `Handler` of `L5W65NestedJarWalkRewrite`, reading the
// outer jar FROM DISK on every open) spans TWO fat jars: three nested jars of
// `one/app.jar` and an `a.jar` of `two/app.jar` (the same inner name in
// another outer: the shared open is keyed by the outer path, not the inner
// name). Round 1 loads one class from each nested jar, misses `p.Missing`,
// reads a resource. Then `one/app.jar` is rewritten at the same path with a
// FOURTH nested jar `d.jar` that no walk row has asked for yet, and a fresh
// loader over all four asks for `s.D`. The new spec's row is built from the
// outer archive: a held open that skipped its re-stat would not list `d.jar`
// and the walk index would call `s.D` absent. This holds with and without
// `CRATONVM_NESTED_JAR_WALK_INDEX_STAMP` (no row of round 1 is asked for a
// name the rewrite added).
//
// Run (no setup; writes two temporary fat jars and deletes them):
//   javac -d out L5W66NestedJarSharedOuter.java
//   CRATONVM_NESTED_JAR_SHARED_OUTER=1 cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W66NestedJarSharedOuter
//   CRATONVM_NESTED_JAR_SHARED_OUTER=1 CRATONVM_NESTED_JAR_BYTES_INDEX=1 cratonvm --java-home <jdk25> -cp out L5W66NestedJarSharedOuter
//   cratonvm --java-home <jdk25> -cp out L5W66NestedJarSharedOuter          # control: per-spec opens
//
// Expected: HotSpot's output on every row, switch on or off. Positive
// control: `CRATONVM_DBG=access` LOADER census `nested-jar-walk-skipped=`
// is the same with the switch on and off (the shared open changes how a row
// is built, never what it says), and non-zero (round 1's `q.B` / `r.C` /
// `p.A2` / `p.Missing` asks skip the jars that lack them).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// class-A-1=loader=true source=one/a
// class-B-1=loader=true source=one/b
// class-C-1=loader=true source=one/c
// class-A2-1=loader=true source=two/a
// absent-1=java.lang.ClassNotFoundException
// res-a-1=alpha
// res-none-1=null
// class-A-2=loader=true source=one/a
// class-D-2=loader=true source=one/d
// res-d-2=delta
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
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipInputStream;
import java.util.zip.ZipOutputStream;

public class L5W66NestedJarSharedOuter {
    /** The outer jars by the raw path their URLs spell. */
    static final Map<String, Path> OUTERS = new ConcurrentHashMap<>();

    /** The stand-in handler's `openConnection`: Spring's not-found rule, read from disk. */
    public static URLConnection open(URL url) throws IOException {
        String file = url.getFile();
        int sep = file.indexOf("!/");
        String nested = file.substring(0, sep);
        String entry = file.substring(sep + 2);
        int split = nested.indexOf("/!");
        String outerRaw = nested.substring("nested:".length(), split);
        String inner = nested.substring(split + 2);
        Path outer = OUTERS.get(outerRaw);
        return new Conn(url, outer == null ? null : read(outer, inner, entry));
    }

    /** `entry` (or `entry + "/"`) of the nested jar `inner` in `outer` as it is on disk now. */
    static byte[] read(Path outerPath, String inner, String entry) throws IOException {
        byte[] nestedBytes;
        try (ZipFile outer = new ZipFile(outerPath.toFile())) {
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
        out.writeByte(1); out.writeUTF("L5W66NestedJarSharedOuter");               // #11
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


    static Map<String, byte[]> nested(String pkg, String cls, String res, String text)
            throws IOException {
        Map<String, byte[]> m = new LinkedHashMap<>();
        m.put(pkg + "/", new byte[0]);
        m.put(pkg + "/" + cls + ".class", emptyClass(pkg + "/" + cls));
        if (res != null) {
            m.put(res, text.getBytes("UTF-8"));
        }
        return m;
    }

    static void writeOne(Path outer, boolean withD) throws IOException {
        // Rewritten in place (same path), as a build tool rewrites its output.
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(outer))) {
            stored(zip, "BOOT-INF/classes/application.properties", "k=v".getBytes("UTF-8"));
            stored(zip, "BOOT-INF/lib/a.jar", jar(nested("p", "A", "a.txt", "alpha")));
            stored(zip, "BOOT-INF/lib/b.jar", jar(nested("q", "B", null, null)));
            stored(zip, "BOOT-INF/lib/c.jar", jar(nested("r", "C", null, null)));
            if (withD) {
                stored(zip, "BOOT-INF/lib/d.jar", jar(nested("s", "D", "d.txt", "delta")));
            }
        }
    }

    static void writeTwo(Path outer) throws IOException {
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(outer))) {
            stored(zip, "BOOT-INF/lib/a.jar", jar(nested("p", "A2", null, null)));
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

    /** `one/a` for a class defined from `.../one/app.jar/!BOOT-INF/lib/a.jar!/`. */
    static String source(Class<?> c) {
        String loc = String.valueOf(c.getProtectionDomain().getCodeSource().getLocation());
        String outer = loc.contains("/one/app.jar/!") ? "one"
                : loc.contains("/two/app.jar/!") ? "two" : "other";
        int at = loc.lastIndexOf("BOOT-INF/lib/");
        String inner = at < 0 || !loc.endsWith(".jar!/") ? "?"
                : loc.substring(at + "BOOT-INF/lib/".length(), loc.length() - ".jar!/".length());
        return outer + "/" + inner;
    }

    static void load(ClassLoader ucl, String label, String name) {
        try {
            Class<?> c = ucl.loadClass(name);
            System.out.println(label + "=loader=" + (c.getClassLoader() == ucl)
                    + " source=" + source(c));
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName());
        }
    }

    static URL url(URLStreamHandler handler, String raw, String inner) throws IOException {
        return new URL(null, "jar:nested:" + raw + "/!BOOT-INF/lib/" + inner + "!/", handler);
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w66-shared-outer");
        Path oneDir = Files.createDirectory(dir.resolve("one"));
        Path twoDir = Files.createDirectory(dir.resolve("two"));
        Path one = oneDir.resolve("app.jar");
        Path two = twoDir.resolve("app.jar");
        String oneRaw = one.toUri().getRawPath().replace("!", "%21");
        String twoRaw = two.toUri().getRawPath().replace("!", "%21");
        OUTERS.put(oneRaw, one);
        OUTERS.put(twoRaw, two);
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W66NestedJarSharedOuter.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        try {
            writeOne(one, false);
            writeTwo(two);
            URL[] first = {
                url(handler, oneRaw, "a.jar"), url(handler, oneRaw, "b.jar"),
                url(handler, oneRaw, "c.jar"), url(handler, twoRaw, "a.jar"),
            };
            try (URLClassLoader ucl = new URLClassLoader(first, ClassLoader.getSystemClassLoader())) {
                load(ucl, "class-A-1", "p.A");
                load(ucl, "class-B-1", "q.B");
                load(ucl, "class-C-1", "r.C");
                load(ucl, "class-A2-1", "p.A2");
                load(ucl, "absent-1", "p.Missing");
                System.out.println("res-a-1=" + text(ucl.getResource("a.txt")));
                System.out.println("res-none-1=" + text(ucl.getResource("none.txt")));
            }
            writeOne(one, true);
            URL[] second = {
                url(handler, oneRaw, "a.jar"), url(handler, oneRaw, "b.jar"),
                url(handler, oneRaw, "c.jar"), url(handler, oneRaw, "d.jar"),
            };
            try (URLClassLoader ucl = new URLClassLoader(second, ClassLoader.getSystemClassLoader())) {
                load(ucl, "class-A-2", "p.A");
                load(ucl, "class-D-2", "s.D");
                System.out.println("res-d-2=" + text(ucl.getResource("d.txt")));
                load(ucl, "absent-2", "p.Missing");
            }
        } finally {
            Files.deleteIfExists(one);
            Files.deleteIfExists(two);
            Files.deleteIfExists(oneDir);
            Files.deleteIfExists(twoDir);
            Files.deleteIfExists(dir);
        }
    }
}
