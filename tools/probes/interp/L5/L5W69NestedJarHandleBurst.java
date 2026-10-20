// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 69, lane L5 -- a fat jar rewritten, replaced,
// truncated and deleted IN THE MIDDLE of a burst of class loads from its
// nested jars, while `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE` (default off;
// over `CRATONVM_NESTED_JAR_BYTES_DIRECTORY` and
// `CRATONVM_NESTED_JAR_BYTES_INDEX`, both default off) may be keeping an
// outer handle open between the loads
// (`docs/internal/fixed-bugs/interpreter-i68-L5-proposal-keep-one-outer-handle-open-per-ask-burst-KEPT-OFF-20261101.md`).
// Every change below happens within a few milliseconds of the previous load
// (inside the 50 ms burst interval), and the replacements keep the file's
// LENGTH and MODIFICATION TIME, so the per-VM rows' stamp still matches: only
// the kept handle's device/inode check (or a CRC mismatch through the old
// directory) stands between a load and stale bytes.
//
// The stand-in `Handler` (Spring Boot's `jar:` handler's class name) reads
// the outer jar FROM DISK on every open, so HotSpot shows the jar as it is
// at each step. `p.A<n>` extends `java.lang.Object` in version 1 and
// `java.lang.Number` in version 2 (the same length, so both fat jars have
// the same layout and size):
//
//   1. v1: A1, A2, A3 load (with the switch, A2 keeps the handle, A3 reuses it);
//   2. REPLACED by a rename with v2, same length and mtime (a new inode): A4
//      is v2's (never the old inode's bytes through the kept handle);
//   3. rewritten IN PLACE with v1, same length and mtime: A5, A6 are v1's;
//   4. rewritten in place with v2 UNDER a kept handle (same inode): A7 is v2's;
//   5. truncated to half: A8 is not found;
//   6. deleted (a kept handle must not make this fail on any platform): A9
//      is not found;
//   7. restored with v1 (after the 100 ms refusal retry interval): A8, A9 load.
//
// Run (no setup; writes one temporary fat jar and deletes it):
//   javac -d out L5W69NestedJarHandleBurst.java
//   B="CRATONVM_NESTED_JAR_BYTES_INDEX=1 CRATONVM_NESTED_JAR_BYTES_DIRECTORY=1"
//   env $B CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE=1 cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W69NestedJarHandleBurst
//   env $B cratonvm --java-home <jdk25> -cp out L5W69NestedJarHandleBurst      # control: open per read
//   cratonvm --java-home <jdk25> -cp out L5W69NestedJarHandleBurst             # defaults (bytes index off)
//   java -cp out L5W69NestedJarHandleBurst                                     # HotSpot
//
// Expected: HotSpot's output on every row. stderr `same-mtime=` says whether
// the file system kept the restored modification times (when false the rows
// were rebuilt by their stamp instead, and the run tests less, not wrongly).
// Positive control: `CRATONVM_DBG=access` LOADER census
// `nested-jar-bytes-served=` is at least 6 (A1-A3, A5, A6, and A8/A9 after
// the restore) with the bytes index on, 0 with it off; with the switch on,
// `nested-jar-outer-handle-reused=` is at least 1 (A3).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// class-A1-1=super=java.lang.Object
// class-A2-1=super=java.lang.Object
// class-A3-1=super=java.lang.Object
// replaced=true
// class-A4-2=super=java.lang.Number
// rewritten-in-place=true
// class-A5-3=super=java.lang.Object
// class-A6-3=super=java.lang.Object
// rewritten-under-handle=true
// class-A7-4=super=java.lang.Number
// truncated=true
// class-A8-5=java.lang.ClassNotFoundException
// deleted=true
// class-A9-6=java.lang.ClassNotFoundException
// restored=true
// class-A8-7=super=java.lang.Object
// class-A9-7=super=java.lang.Object
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
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.nio.file.attribute.FileTime;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipInputStream;
import java.util.zip.ZipOutputStream;

public class L5W69NestedJarHandleBurst {

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
        out.writeByte(1); out.writeUTF("L5W69NestedJarHandleBurst");             // #11
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

    /** A fixed entry time inside the DOS range: no extra field, the same layout every time. */
    static final long ENTRY_TIME = 1_700_000_000_000L;

    /** `public class <name> extends <superName> {}` (version 49). */
    static byte[] emptyClass(String name, String superName) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(5);
        out.writeByte(1); out.writeUTF(name);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF(superName);
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

    static void stored(ZipOutputStream zip, String name, byte[] data) throws IOException {
        ZipEntry entry = new ZipEntry(name);
        entry.setMethod(ZipEntry.STORED);
        entry.setTime(ENTRY_TIME);
        entry.setSize(data.length);
        entry.setCompressedSize(data.length);
        CRC32 crc = new CRC32();
        crc.update(data);
        entry.setCrc(crc.getValue());
        zip.putNextEntry(entry);
        zip.write(data);
        zip.closeEntry();
    }

    /** `a.jar`: `p/A1`..`p/A9`, every entry STORED, extending `superName`. */
    static byte[] nestedA(String superName) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            stored(zip, "p/", new byte[0]);
            for (int i = 1; i <= 9; i++) {
                stored(zip, "p/A" + i + ".class", emptyClass("p/A" + i, superName));
            }
        }
        return bytes.toByteArray();
    }

    /** `b.jar`: `q/B`. */
    static byte[] nestedB() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            stored(zip, "q/", new byte[0]);
            stored(zip, "q/B.class", emptyClass("q/B", "java/lang/Object"));
        }
        return bytes.toByteArray();
    }

    /** The fat jar; `v2`: every `p.A<n>` extends `java.lang.Number` (same length). */
    static byte[] fatJar(boolean v2) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            stored(zip, "BOOT-INF/classes/application.properties", "k=v".getBytes("UTF-8"));
            stored(zip, "BOOT-INF/lib/a.jar", nestedA(v2 ? "java/lang/Number" : "java/lang/Object"));
            stored(zip, "BOOT-INF/lib/b.jar", nestedB());
        }
        return bytes.toByteArray();
    }

    static String describe(ClassLoader ucl, String name) {
        try {
            Class<?> c = ucl.loadClass(name);
            return "super=" + c.getSuperclass().getName();
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static void load(ClassLoader ucl, String label, String name) {
        System.out.println(label + "=" + describe(ucl, name));
    }

    static URL url(URLStreamHandler handler, String raw, String inner) throws IOException {
        return new URL(null, "jar:nested:" + raw + "/!BOOT-INF/lib/" + inner + "!/", handler);
    }

    static boolean sameMtime = true;

    /** Put `time` back on `path`, noting whether the file system kept it. */
    static void restoreMtime(Path path, FileTime time) throws IOException {
        Files.setLastModifiedTime(path, time);
        sameMtime &= Files.getLastModifiedTime(path).equals(time);
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w69-handle-burst");
        Path oneDir = Files.createDirectory(dir.resolve("one"));
        Path one = oneDir.resolve("app.jar");
        Path next = oneDir.resolve("app.jar.next");
        String oneRaw = one.toUri().getRawPath().replace("!", "%21");
        OUTERS.put(oneRaw, one);
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W69NestedJarHandleBurst.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        byte[] v1 = fatJar(false);
        byte[] v2 = fatJar(true);
        if (v1.length != v2.length) {
            throw new AssertionError("the two versions must have one length: " + v1.length + " vs " + v2.length);
        }
        URL[] urls = { url(handler, oneRaw, "a.jar"), url(handler, oneRaw, "b.jar") };
        try {
            Files.write(one, v1);
            FileTime mtime = Files.getLastModifiedTime(one);
            try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                // 1. A burst over v1.
                load(ucl, "class-A1-1", "p.A1");
                load(ucl, "class-A2-1", "p.A2");
                load(ucl, "class-A3-1", "p.A3");

                // 2. Replaced by a rename: same length, same mtime, new inode.
                Files.write(next, v2);
                restoreMtime(next, mtime);
                Files.move(next, one, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
                System.out.println("replaced=" + (Files.size(one) == v1.length));
                load(ucl, "class-A4-2", "p.A4");

                // 3. Rewritten in place with v1: same length, same mtime.
                Files.write(one, v1, StandardOpenOption.WRITE, StandardOpenOption.TRUNCATE_EXISTING);
                restoreMtime(one, mtime);
                System.out.println("rewritten-in-place=" + (Files.size(one) == v1.length));
                load(ucl, "class-A5-3", "p.A5");
                load(ucl, "class-A6-3", "p.A6");

                // 4. Rewritten in place with v2 under a kept handle (same inode).
                Files.write(one, v2, StandardOpenOption.WRITE, StandardOpenOption.TRUNCATE_EXISTING);
                restoreMtime(one, mtime);
                System.out.println("rewritten-under-handle=" + (Files.size(one) == v2.length));
                load(ucl, "class-A7-4", "p.A7");

                // 5. Truncated to half in place.
                try (FileChannel ch = FileChannel.open(one, StandardOpenOption.WRITE)) {
                    ch.truncate(v2.length / 2);
                }
                System.out.println("truncated=" + (Files.size(one) == v2.length / 2));
                load(ucl, "class-A8-5", "p.A8");

                // 6. Deleted: must succeed whatever the VM holds.
                Files.delete(one);
                System.out.println("deleted=" + !Files.exists(one));
                load(ucl, "class-A9-6", "p.A9");

                // 7. Restored with v1, past the refusal retry interval.
                Thread.sleep(250);
                Files.write(one, v1);
                System.out.println("restored=" + (Files.size(one) == v1.length));
                Thread.sleep(250);
                load(ucl, "class-A8-7", "p.A8");
                load(ucl, "class-A9-7", "p.A9");
            }
            System.err.println("same-mtime=" + sameMtime);
        } finally {
            Files.deleteIfExists(next);
            Files.deleteIfExists(one);
            Files.deleteIfExists(oneDir);
            Files.deleteIfExists(dir);
        }
    }
}
