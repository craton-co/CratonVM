// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 68, lane L5 -- a nested-jar walk-index or
// bytes-index row REFUSED while its fat jar was half written gets its fast
// path back once the jar is whole again (`CRATONVM_NESTED_JAR_REFUSAL_STAMP`,
// default off: a refusal records the outer file's stamp read before the
// refused build and is rebuilt -- at most once per 100 ms per row -- by an ask
// that finds the file changed; a bytes-index row whose jar was rewritten is
// rebuilt instead of refused;
// `docs/internal/fixed-bugs/interpreter-i67-L5-proposal-stamp-a-nested-jar-index-refusal-DONE-20261031.md`).
//
// The stand-in `Handler` (Spring Boot's `jar:` handler's class name, so the
// JDK's generic `URLClassPath$Loader` serves the `jar:nested:` URLs) reads
// the outer jar FROM DISK on every open; what HotSpot shows is the jar as it
// is at each step:
//
//   1. as built: `p.A` loads, `p.Missing` is absent, `a.txt` reads;
//   2. truncated to half in place: `q.B`, `p.A3` are not found, `a.txt` is
//      `null` (in CratonVM every row is rebuilt from the half jar and refused);
//   3. rewritten whole with `q.B2` added (after the retry interval): `q.B`,
//      `q.B2`, `p.A3` load, `p.Missing3` is absent, `a.txt` reads;
//   4. rewritten back without `q.B2`, a fresh loader: `q.B` loads, `q.B2` is
//      not found (a rebuilt row must follow the file, never answer the old
//      one), `a.txt` reads;
//   5. truncated to 22 bytes, then deleted, then restored with `q.B2` (a
//      fresh loader, asks between each step): `r.C` is not found twice, then
//      loads; `q.B2` loads; `r.Missing5` is absent.
//
// Run (no setup; writes one temporary fat jar and deletes it):
//   javac -d out L5W68NestedJarRefusalRetry.java
//   CRATONVM_NESTED_JAR_REFUSAL_STAMP=1 cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W68NestedJarRefusalRetry
//   CRATONVM_NESTED_JAR_REFUSAL_STAMP=1 CRATONVM_NESTED_JAR_BYTES_INDEX=1 cratonvm --java-home <jdk25> -cp out L5W68NestedJarRefusalRetry
//   CRATONVM_NESTED_JAR_REFUSAL_STAMP=1 CRATONVM_NESTED_JAR_BYTES_INDEX=1 CRATONVM_NESTED_JAR_BYTES_DIRECTORY=1 cratonvm --java-home <jdk25> -cp out L5W68NestedJarRefusalRetry
//   cratonvm --java-home <jdk25> -cp out L5W68NestedJarRefusalRetry          # control: refusals kept for the VM's life
//   java -cp out L5W68NestedJarRefusalRetry                                  # HotSpot
//
// Expected: HotSpot's output on every row (the switch changes which asks run
// the JDK body, never an answer). Positive control: `CRATONVM_DBG=access`
// LOADER census `nested-jar-walk-skipped=`: the control row stops counting
// after step 1 (3: `p.Missing` skips a.jar, b.jar, c.jar; every row is a
// refusal from step 2 on), the REFUSAL_STAMP rows keep counting through
// steps 3-5 (`p.Missing3`, `q.B2` in step 4 and `r.Missing5` are skips again:
// at least 3 + 3 + 1 + 3 = 10). With the bytes index on, the census
// `nested-jar-bytes-served=` likewise keeps growing after step 2 only with
// the switch.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// class-A-1=loader=true source=one/a
// absent-1=java.lang.ClassNotFoundException
// res-a-1=alpha
// truncated=true
// class-B-2=java.lang.ClassNotFoundException
// class-A3-2=java.lang.ClassNotFoundException
// res-a-2=null
// rewritten=true
// class-B-3=loader=true source=one/b
// class-B2-3=loader=true source=one/b
// class-A3-3=loader=true source=one/a
// absent-3=java.lang.ClassNotFoundException
// res-a-3=alpha
// rewritten-back=true
// class-B-4=loader=true source=one/b
// class-B2-4=java.lang.ClassNotFoundException
// res-a-4=alpha
// truncated-5=true
// class-C-5a=java.lang.ClassNotFoundException
// deleted-5=true
// class-C-5b=java.lang.ClassNotFoundException
// restored-5=true
// class-C-5c=loader=true source=one/c
// class-B2-5=loader=true source=one/b
// absent-5=java.lang.ClassNotFoundException
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
import java.nio.file.StandardOpenOption;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipInputStream;
import java.util.zip.ZipOutputStream;

public class L5W68NestedJarRefusalRetry {

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
        out.writeByte(1); out.writeUTF("L5W68NestedJarRefusalRetry");             // #11
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

    /** A nested jar with a package directory, classes, and an optional resource. */
    static byte[] nested(String pkg, String[] classes, String res, String text) throws IOException {
        Map<String, byte[]> m = new LinkedHashMap<>();
        m.put(pkg + "/", new byte[0]);
        for (String cls : classes) {
            m.put(pkg + "/" + cls + ".class", emptyClass(pkg + "/" + cls));
        }
        if (res != null) {
            m.put(res, text.getBytes("UTF-8"));
        }
        return jar(m);
    }

    /** The fat jar's bytes; `withB2` adds `q.B2` to `b.jar`. */
    static byte[] fatJar(boolean withB2) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            stored(zip, "BOOT-INF/classes/application.properties", "k=v".getBytes("UTF-8"));
            stored(zip, "BOOT-INF/lib/a.jar", nested("p", new String[] {"A", "A3"}, "a.txt", "alpha"));
            stored(zip, "BOOT-INF/lib/b.jar",
                    nested("q", withB2 ? new String[] {"B", "B2"} : new String[] {"B"}, null, null));
            stored(zip, "BOOT-INF/lib/c.jar", nested("r", new String[] {"C"}, null, null));
        }
        return bytes.toByteArray();
    }

    /** Rewritten IN PLACE (same path, same inode): truncate, then write. */
    static void rewrite(Path outer, byte[] bytes) throws IOException {
        Files.write(outer, bytes, StandardOpenOption.CREATE, StandardOpenOption.TRUNCATE_EXISTING,
                StandardOpenOption.WRITE);
    }

    /** Truncated IN PLACE to `size` bytes. */
    static void truncate(Path outer, long size) throws IOException {
        try (FileChannel ch = FileChannel.open(outer, StandardOpenOption.WRITE)) {
            ch.truncate(size);
        }
    }

    static String text(URL url) {
        if (url == null) {
            return "null";
        }
        try (InputStream in = url.openStream()) {
            return new String(in.readAllBytes(), "UTF-8");
        } catch (IOException e) {
            return e.getClass().getName();
        }
    }

    /** `one/a` for a class defined from `.../one/app.jar/!BOOT-INF/lib/a.jar!/`. */
    static String source(Class<?> c) {
        String loc = String.valueOf(c.getProtectionDomain().getCodeSource().getLocation());
        String outer = loc.contains("/one/app.jar/!") ? "one" : "other";
        int at = loc.lastIndexOf("BOOT-INF/lib/");
        String inner = at < 0 || !loc.endsWith(".jar!/") ? "?"
                : loc.substring(at + "BOOT-INF/lib/".length(), loc.length() - ".jar!/".length());
        return outer + "/" + inner;
    }

    static String describe(ClassLoader ucl, String name) {
        try {
            Class<?> c = ucl.loadClass(name);
            return "loader=" + (c.getClassLoader() == ucl) + " source=" + source(c);
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

    /** Long enough for a refused row's retry interval (100 ms) to pass. */
    static void pastTheRetryInterval() throws InterruptedException {
        Thread.sleep(250);
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w68-refusal-retry");
        Path oneDir = Files.createDirectory(dir.resolve("one"));
        Path one = oneDir.resolve("app.jar");
        String oneRaw = one.toUri().getRawPath().replace("!", "%21");
        OUTERS.put(oneRaw, one);
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W68NestedJarRefusalRetry.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        byte[] first = fatJar(false);
        byte[] second = fatJar(true);
        URL[] urls = {
            url(handler, oneRaw, "a.jar"), url(handler, oneRaw, "b.jar"), url(handler, oneRaw, "c.jar"),
        };
        try {
            rewrite(one, first);
            try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                // 1. As built: every nested jar gets a walk row.
                load(ucl, "class-A-1", "p.A");
                load(ucl, "absent-1", "p.Missing");
                System.out.println("res-a-1=" + text(ucl.getResource("a.txt")));

                // 2. Truncated to half in place: every row is rebuilt and refused.
                truncate(one, first.length / 2);
                System.out.println("truncated=" + (Files.size(one) == first.length / 2));
                load(ucl, "class-B-2", "q.B");
                load(ucl, "class-A3-2", "p.A3");
                System.out.println("res-a-2=" + text(ucl.getResource("a.txt")));

                // 3. Whole again (one more class): a stamped refusal is rebuilt.
                pastTheRetryInterval();
                rewrite(one, second);
                System.out.println("rewritten=" + (Files.size(one) == second.length));
                pastTheRetryInterval();
                load(ucl, "class-B-3", "q.B");
                load(ucl, "class-B2-3", "q.B2");
                load(ucl, "class-A3-3", "p.A3");
                load(ucl, "absent-3", "p.Missing3");
                System.out.println("res-a-3=" + text(ucl.getResource("a.txt")));
            }

            // 4. Rewritten back (without q.B2): the rebuilt rows follow the file.
            rewrite(one, first);
            System.out.println("rewritten-back=" + (Files.size(one) == first.length));
            try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                load(ucl, "class-B-4", "q.B");
                load(ucl, "class-B2-4", "q.B2");
                System.out.println("res-a-4=" + text(ucl.getResource("a.txt")));
            }

            // 5. Truncated, deleted, then restored with q.B2: refused while
            //    broken or absent, served again once the jar is back.
            try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                truncate(one, 22);
                System.out.println("truncated-5=" + (Files.size(one) == 22));
                load(ucl, "class-C-5a", "r.C");
                pastTheRetryInterval();
                Files.delete(one);
                System.out.println("deleted-5=" + !Files.exists(one));
                load(ucl, "class-C-5b", "r.C");
                pastTheRetryInterval();
                rewrite(one, second);
                System.out.println("restored-5=" + (Files.size(one) == second.length));
                pastTheRetryInterval();
                load(ucl, "class-C-5c", "r.C");
                load(ucl, "class-B2-5", "q.B2");
                load(ucl, "absent-5", "r.Missing5");
            }
        } finally {
            Files.deleteIfExists(one);
            Files.deleteIfExists(oneDir);
            Files.deleteIfExists(dir);
        }
    }
}
