// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 70, lane L5 -- `URLClassPath$JarLoader` reads of
// a jar that is ON THE VM'S APPLICATION CLASS PATH, with
// `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE` (default off): the jar-loader
// bytes row of that jar is built from the class path's own mapping and parse
// (`ClassPath::outer_archive_for`, `ClassPath::open_jar_archive_lent`)
// instead of mapping and parsing the file a second time
// (`docs/internal/fixed-bugs/interpreter-i69-L5-proposal-lend-the-class-path-archive-to-the-jar-loader-bytes-index-KEPT-OFF-20261104.md`).
// What HotSpot shows is the jar as it is on disk when each loader opens it,
// so a jar changed since the class path loaded it must be read as it now is.
// Each step uses a FRESH `URLClassLoader` over the jar's `file:` URL (parent:
// the platform loader, so the application loader -- which has the same jar
// on `-cp` -- never answers), closed before the next step:
//
//   1. as loaded: p.A1 loads (extends Object), p.Missing is absent;
//   2. REWRITTEN IN PLACE with the same length (p.A<n> extend Number; only
//      the classes' CRCs differ): p.A2 is the new one's (super Number);
//   3. REPLACED by a rename with another length (plus p.New): p.A3 and
//      p.New load (Number);
//   4. deleted: p.A4 is not found;
//   5. restored with v1 (past the 100 ms refusal retry interval): p.A4
//      loads (Object).
//
// How a changed jar is detected with the switch: the class path's entry
// keeps the file's length and modification time read when it loaded the jar;
// a row is lent only while a `stat` of the file still reads that stamp, the
// row carries that load-time stamp so the memo's re-`stat` on every hit drops
// it once the file changes (steps 2-5 are new rows opened from the file as
// without the switch), and every served entry must also match the CRC and
// size of the JDK's own `ZipEntry`.
//
// The jar must exist BEFORE the VM starts (it is on `-cp`), so the probe runs
// in two steps: `--make <jar>` writes it (any JVM), then the probe runs with
// the jar appended to the class path and finds it there (the element ending
// in `l5w70lib.jar`). Linux/macOS only: on Windows the application loader
// (HotSpot's too) may keep a class-path jar open without FILE_SHARE_DELETE,
// so steps 3-5 fail there on every VM.
//
// Run:
//   javac -d out L5W70JarLoaderClassPathArchive.java
//   java -cp out L5W70JarLoaderClassPathArchive --make /tmp/l5w70cp/l5w70lib.jar
//   B="CRATONVM_JAR_LOADER_BYTES_INDEX=1"
//   env $B CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE=1 cratonvm --java-home <jdk25> [--compatible] -cp out:/tmp/l5w70cp/l5w70lib.jar L5W70JarLoaderClassPathArchive
//   env $B cratonvm --java-home <jdk25> [--compatible] -cp out:/tmp/l5w70cp/l5w70lib.jar L5W70JarLoaderClassPathArchive   # control: a second mapping
//   java -cp out:/tmp/l5w70cp/l5w70lib.jar L5W70JarLoaderClassPathArchive                                               # HotSpot
// (re-run `--make` before each row: step 5 restores v1, so a row leaves the
// jar as it found it, but a crashed row may not.)
//
// Expected: HotSpot's output on every row. Positive control:
// `CRATONVM_DBG=access` LOADER census `jar-loader-class-path-lent=1` (the
// step-1 row; every later row is opened from the file) and
// `jar-loader-bytes-served=` >= 5 where the bytes index is registered (the
// `URLClassLoader` family runs the JDK bodies); `jar-loader-class-path-lent=0`
// without the switch.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// on-class-path=true
// class-A1-1=super=java.lang.Object
// absent-1=java.lang.ClassNotFoundException
// rewritten-same-length=true
// class-A2-2=super=java.lang.Number
// replaced=true
// class-A3-3=super=java.lang.Number
// class-New-3=super=java.lang.Number
// deleted=true
// class-A4-4=java.lang.ClassNotFoundException
// restored=true
// class-A4-5=super=java.lang.Object
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.nio.file.StandardCopyOption;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W70JarLoaderClassPathArchive {

    /** A fixed entry time inside the DOS range, so every version has the same layout. */
    static final long ENTRY_TIME = 1577836800000L;

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

    /**
     * The jar: `p/A1`..`p/A4` extending Object (v1) or Number (`number`;
     * "java/lang/Number" is as long as "java/lang/Object", so the two have
     * the same length), plus `p/New` when `extra` (another length).
     */
    static byte[] jar(boolean number, boolean extra) throws IOException {
        String superName = number ? "java/lang/Number" : "java/lang/Object";
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            stored(zip, "p/", new byte[0]);
            for (int i = 1; i <= 4; i++) {
                stored(zip, "p/A" + i + ".class", emptyClass("p/A" + i, superName));
            }
            if (extra) {
                stored(zip, "p/New.class", emptyClass("p/New", superName));
            }
        }
        return bytes.toByteArray();
    }

    static String describe(ClassLoader loader, String name) {
        try {
            Class<?> c = loader.loadClass(name);
            return "super=" + c.getSuperclass().getName();
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    /** Load each of `names` (label, class name pairs) through a fresh loader over `jar`, then close it. */
    static void step(Path jar, String... labelsAndNames) throws IOException {
        URL[] urls = {jar.toUri().toURL()};
        try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            for (int i = 0; i < labelsAndNames.length; i += 2) {
                System.out.println(labelsAndNames[i] + "=" + describe(ucl, labelsAndNames[i + 1]));
            }
        }
    }

    /** The class path element ending in `l5w70lib.jar` (absolute), or null. */
    static Path jarOnClassPath() {
        for (String element : System.getProperty("java.class.path").split(File.pathSeparator)) {
            if (element.endsWith("l5w70lib.jar")) {
                return Paths.get(element).toAbsolutePath();
            }
        }
        return null;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("--make")) {
            Path jar = Paths.get(args[1]).toAbsolutePath();
            Files.createDirectories(jar.getParent());
            Files.write(jar, jar(false, false));
            System.out.println("made " + jar);
            return;
        }
        Path jar = jarOnClassPath();
        System.out.println("on-class-path=" + (jar != null && Files.isRegularFile(jar)));
        if (jar == null) {
            return;
        }
        byte[] v1 = jar(false, false);
        byte[] sameLength = jar(true, false);
        byte[] longer = jar(true, true);
        Path next = jar.resolveSibling("l5w70lib.jar.next");
        try {
            // 1. As the class path loaded it.
            step(jar, "class-A1-1", "p.A1", "absent-1", "p.Missing");

            // 2. Rewritten in place, same length (a later modification time).
            Thread.sleep(50);
            Files.write(jar, sameLength);
            System.out.println("rewritten-same-length=" + (Files.size(jar) == v1.length
                    && sameLength.length == v1.length));
            step(jar, "class-A2-2", "p.A2");

            // 3. Replaced by a rename with another length.
            Files.write(next, longer);
            Files.move(next, jar, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
            System.out.println("replaced=" + (Files.size(jar) == longer.length));
            step(jar, "class-A3-3", "p.A3", "class-New-3", "p.New");

            // 4. Deleted.
            Files.delete(jar);
            System.out.println("deleted=" + !Files.exists(jar));
            step(jar, "class-A4-4", "p.A4");

            // 5. Restored as the class path loaded it, past the retry interval.
            Thread.sleep(250);
            Files.write(next, v1);
            Files.move(next, jar, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
            System.out.println("restored=" + (Files.size(jar) == v1.length));
            Thread.sleep(250);
            step(jar, "class-A4-5", "p.A4");
        } finally {
            Files.deleteIfExists(next);
        }
    }
}
