// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 69, lane L5 -- `jar:nested:` URLs over a fat
// jar that is ON THE VM'S APPLICATION CLASS PATH (the `java -jar app.jar`
// shape), with `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER` (default off): the
// nested-jar walk and bytes rows of that fat jar are built from the class
// path's own bytes and parse instead of opening the file again
// (`docs/internal/proposals/interpreter/i66-L5-proposal-reuse-the-class-path-parse-of-a-fat-jar-20261030.md`).
// What HotSpot shows is the jar as it is on disk at each step, so a fat jar
// replaced since the class path loaded it must be read as it now is:
//
//   1. as loaded: p.A1, p.A2 load (extend Object), a.txt reads, p.Missing is absent;
//   2. REPLACED by a rename with v2 (another length: p.A<n> extend Number,
//      plus p.New): p.A3 is v2's, p.New loads, a.txt reads v2's text;
//   3. deleted: p.A4 is not found, a.txt is null;
//   4. restored with v1 (past the 100 ms refusal retry interval): p.A4 loads (Object).
//
// The fat jar must exist BEFORE the VM starts (it is on `-cp`), so the probe
// runs in two steps: `--make <jar>` writes it (any JVM), then the probe runs
// with the jar appended to the class path and finds it there.
// The nested jar sits at `lib/a.jar`, not under `BOOT-INF/`, so that no
// CratonVM mode applies fat-jar emulation to the class path (which would
// let the application loader itself answer `p.A<n>` from its own copy):
// the jar is a plain class-path archive everywhere, as on HotSpot.
// Linux/macOS only: on Windows the JDK's own application loader (HotSpot's
// too) keeps a class-path jar open without FILE_SHARE_DELETE, so step 2's
// replace fails there on every VM.
//
// Run:
//   javac -d out L5W69NestedJarClassPathOuter.java
//   java -cp out L5W69NestedJarClassPathOuter --make /tmp/l5w69cp/app.jar
//   B="CRATONVM_NESTED_JAR_BYTES_INDEX=1"
//   env $B CRATONVM_NESTED_JAR_CLASS_PATH_OUTER=1 cratonvm --java-home <jdk25> [--compatible] -cp out:/tmp/l5w69cp/app.jar L5W69NestedJarClassPathOuter
//   env $B cratonvm --java-home <jdk25> -cp out:/tmp/l5w69cp/app.jar L5W69NestedJarClassPathOuter      # control: the outer index opens the file
//   java -cp out:/tmp/l5w69cp/app.jar L5W69NestedJarClassPathOuter                                     # HotSpot
// (re-run `--make` before each row: step 4 restores v1, so a row leaves the
// jar as it found it, but a crashed row may not.)
//
// Expected: HotSpot's output on every row. Positive control:
// `CRATONVM_DBG=access` LOADER census `nested-jar-bytes-served=` >= 3 with
// the bytes index on; with the switch, `strace -f -e trace=openat` shows no
// `openat` of app.jar after the class path's own (none for the step-1 rows),
// and the step-2 rows open it again (the class path's stamp no longer
// matches).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// on-class-path=true
// class-A1-1=super=java.lang.Object
// class-A2-1=super=java.lang.Object
// res-a-1=alpha-1
// absent-1=java.lang.ClassNotFoundException
// replaced=true
// class-A3-2=super=java.lang.Number
// class-New-2=super=java.lang.Number
// res-a-2=alpha-2
// deleted=true
// class-A4-3=java.lang.ClassNotFoundException
// res-a-3=null
// restored=true
// class-A4-4=super=java.lang.Object
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
import java.nio.file.StandardCopyOption;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipInputStream;
import java.util.zip.ZipOutputStream;

public class L5W69NestedJarClassPathOuter {

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
        out.writeByte(1); out.writeUTF("L5W69NestedJarClassPathOuter");             // #11
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
        entry.setSize(data.length);
        entry.setCompressedSize(data.length);
        CRC32 crc = new CRC32();
        crc.update(data);
        entry.setCrc(crc.getValue());
        zip.putNextEntry(entry);
        zip.write(data);
        zip.closeEntry();
    }

    /** `a.jar`: `p/A1`..`p/A4` (and `p/New` in v2) extending `superName`, and `a.txt`. */
    static byte[] nestedA(boolean v2) throws IOException {
        String superName = v2 ? "java/lang/Number" : "java/lang/Object";
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            stored(zip, "p/", new byte[0]);
            for (int i = 1; i <= 4; i++) {
                stored(zip, "p/A" + i + ".class", emptyClass("p/A" + i, superName));
            }
            if (v2) {
                stored(zip, "p/New.class", emptyClass("p/New", superName));
            }
            stored(zip, "a.txt", (v2 ? "alpha-2" : "alpha-1").getBytes("UTF-8"));
        }
        return bytes.toByteArray();
    }

    /** The fat jar: a stored nested jar, outside BOOT-INF/ so no VM mode emulates a fat jar. */
    static byte[] fatJar(boolean v2) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            stored(zip, "classes/application.properties", "k=v".getBytes("UTF-8"));
            stored(zip, "lib/a.jar", nestedA(v2));
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

    /** The class path element named `app.jar` (absolute), or null. */
    static Path fatJarOnClassPath() {
        for (String element : System.getProperty("java.class.path").split(File.pathSeparator)) {
            if (element.endsWith("app.jar")) {
                return Paths.get(element).toAbsolutePath();
            }
        }
        return null;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("--make")) {
            Path jar = Paths.get(args[1]).toAbsolutePath();
            Files.createDirectories(jar.getParent());
            Files.write(jar, fatJar(false));
            System.out.println("made " + jar);
            return;
        }
        Path one = fatJarOnClassPath();
        System.out.println("on-class-path=" + (one != null && Files.isRegularFile(one)));
        if (one == null) {
            return;
        }
        String oneRaw = one.toUri().getRawPath().replace("!", "%21");
        OUTERS.put(oneRaw, one);
        URLStreamHandler handler = (URLStreamHandler) new HandlerLoader(
                L5W69NestedJarClassPathOuter.class.getClassLoader()).define(handlerClass())
                .getDeclaredConstructor().newInstance();
        byte[] v1 = fatJar(false);
        byte[] v2 = fatJar(true);
        Path next = one.resolveSibling("app.jar.next");
        URL[] urls = { new URL(null, "jar:nested:" + oneRaw + "/!lib/a.jar!/", handler) };
        try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
            // 1. As the class path loaded it.
            load(ucl, "class-A1-1", "p.A1");
            load(ucl, "class-A2-1", "p.A2");
            System.out.println("res-a-1=" + text(ucl.getResource("a.txt")));
            load(ucl, "absent-1", "p.Missing");

            // 2. Replaced by a rename with another version (another length).
            Files.write(next, v2);
            Files.move(next, one, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
            System.out.println("replaced=" + (Files.size(one) == v2.length));
            load(ucl, "class-A3-2", "p.A3");
            load(ucl, "class-New-2", "p.New");
            System.out.println("res-a-2=" + text(ucl.getResource("a.txt")));

            // 3. Deleted.
            Files.delete(one);
            System.out.println("deleted=" + !Files.exists(one));
            load(ucl, "class-A4-3", "p.A4");
            System.out.println("res-a-3=" + text(ucl.getResource("a.txt")));

            // 4. Restored as the class path loaded it, past the retry interval.
            Thread.sleep(250);
            Files.write(next, v1);
            Files.move(next, one, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
            System.out.println("restored=" + (Files.size(one) == v1.length));
            Thread.sleep(250);
            load(ucl, "class-A4-4", "p.A4");
        } finally {
            Files.deleteIfExists(next);
        }
    }
}
