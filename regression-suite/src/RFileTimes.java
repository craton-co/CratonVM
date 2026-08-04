// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.file.Files;
import java.nio.file.attribute.BasicFileAttributeView;
import java.nio.file.attribute.BasicFileAttributes;
import java.nio.file.attribute.FileTime;
import java.time.Instant;
import java.util.Enumeration;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

/**
 * File and ZIP-entry timestamp round-trips.
 *
 * This is the Spring Boot `jarmode-tools` extract pipeline reduced to its
 * timestamp-carrying steps, with no Spring on the classpath. Both
 * `ExtractCommandTests.appliesFileTimes` and
 * `ExtractLayersCommandTests.run*ExtractsLayers` come down to: write a jar
 * whose entries carry explicit FileTimes, read those entries back, extract
 * them, push the entry's time onto the extracted file with
 * `BasicFileAttributeView.setTimes`, and then assert the file reports it
 * through `BasicFileAttributeView.readAttributes().lastModifiedTime()`.
 *
 * The defect this gates (2026-08-04): `readAttributes()` answered 1970-01-01
 * for every time on every Unix host. `setTimes` had written the inode
 * correctly -- `stat` proved it -- but the attributes carrier stored and
 * loaded its three timestamps under `st_birthtime`/`st_atime`/`st_mtime`,
 * names the real `sun.nio.fs.UnixFileAttributes` has never declared (it uses
 * `st_mtime_sec`/`st_mtime_nsec` pairs). A `set_field_by_name` on an
 * undeclared name is a silent no-op, so the store and the load agreed with
 * each other and with nothing else. Stage 1 alone catches it; the later
 * stages keep the surrounding pipeline honest.
 *
 * Two more divergences this gates, both closed 2026-08-04 (second round):
 *
 * 1. `ZipEntry.getLastAccessTime()`/`getCreationTime()` answered real values
 *    where HotSpot answers null. The JDK writes the 0x5455 extended-timestamp
 *    field twice with different payloads -- all three times in the local
 *    header, the modified time alone in the central directory -- and
 *    ZipFile/JarFile read the central directory. We were reading the local
 *    header on top of it.
 * 2. `JarFile.entries()` reported 1979-11-30T00:00:16Z for an entry carrying
 *    only a DOS timestamp. The Spring Boot loader bridge dual-writes synthetic
 *    slot 1, which is `xdostime` in the real JDK layout, so the entry's SIZE
 *    landed in the timestamp field (size 8 -> DOS seconds 16). Only
 *    `getLastModifiedTime()` on an entry with no FileTime reads that field, so
 *    nothing else noticed.
 *
 * Whole-second instants only: NTFS, ext4 and APFS disagree below a second, and
 * this vector is diffed against HotSpot byte for byte. DOS timestamps have
 * two-second resolution, so DOS_ONLY_TIME lands on an even second.
 *
 * Deliberately NOT printed: a file's `creationTime()` -- settable on Windows,
 * ignored by the Linux kernel, so HotSpot itself answers differently per host.
 */
public class RFileTimes {

    static final Instant CREATED = Instant.parse("2020-01-01T00:00:00Z");
    static final Instant MODIFIED = Instant.parse("2021-01-01T00:00:00Z");
    static final Instant ACCESSED = Instant.parse("2022-01-01T00:00:00Z");

    /** Even second: DOS timestamps cannot represent an odd one. */
    static final Instant DOS_ONLY_TIME = Instant.parse("2021-06-15T12:34:56Z");

    static final String[] NAMES = {
        "BOOT-INF/classpath.idx", "BOOT-INF/lib/dependency-1.jar", "BOOT-INF/classes/app.properties",
    };

    /**
     * Written with `setTime` alone, which sets `xdostime` and clears `mtime`,
     * so `ZipOutputStream` emits no 0x5455 field for it and the read side has
     * nothing but the DOS timestamp to answer from.
     */
    static final String DOS_ONLY = "BOOT-INF/classes/dos-only.properties";

    static int checks = 0;

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("rfiletimes").toFile();
        try {
            plainFileRoundTrip(dir);
            File archive = writeArchive(dir);
            entryTimesRoundTrip(archive);
            extractedFileTimes(dir, archive);
            System.out.println("CK checks " + checks);
            System.out.println("PASS RFileTimes");
        }
        finally {
            deleteTree(dir);
        }
    }

    /** Stage 1: setTimes -> readAttributes on an ordinary file. */
    static void plainFileRoundTrip(File dir) throws IOException {
        File f = new File(dir, "plain.txt");
        try (OutputStream out = new FileOutputStream(f)) {
            out.write("plain".getBytes("UTF-8"));
        }
        BasicFileAttributeView view = Files.getFileAttributeView(f.toPath(), BasicFileAttributeView.class);
        view.setTimes(FileTime.from(MODIFIED), FileTime.from(ACCESSED), FileTime.from(CREATED));
        BasicFileAttributes attrs = view.readAttributes();
        emit("plain.readAttributes.lastModified", attrs.lastModifiedTime().toInstant());
        emit("plain.readAttributes.lastAccess", attrs.lastAccessTime().toInstant());
        emit("plain.readAttributes.size", attrs.size());
        emit("plain.readAttributes.isRegularFile", attrs.isRegularFile());
        // The same value through the two other JDK spellings, which reach
        // different natives in this VM.
        emit("plain.Files.getLastModifiedTime", Files.getLastModifiedTime(f.toPath()).toInstant());
        emit("plain.File.lastModified", Instant.ofEpochMilli(f.lastModified()));
        // A directory must answer through the same carrier.
        BasicFileAttributes dirAttrs = Files
            .getFileAttributeView(dir.toPath(), BasicFileAttributeView.class)
            .readAttributes();
        emit("dir.isDirectory", dirAttrs.isDirectory());
    }

    /** Stage 2: JarOutputStream writes entries carrying explicit FileTimes. */
    static File writeArchive(File dir) throws IOException {
        File archive = new File(dir, "test.jar");
        try (JarOutputStream jar = new JarOutputStream(new FileOutputStream(archive))) {
            for (String name : NAMES) {
                ZipEntry entry = new ZipEntry(name);
                entry.setCreationTime(FileTime.from(CREATED));
                entry.setLastModifiedTime(FileTime.from(MODIFIED));
                entry.setLastAccessTime(FileTime.from(ACCESSED));
                jar.putNextEntry(entry);
                jar.write(("content-of-" + name).getBytes("UTF-8"));
                jar.closeEntry();
            }
            ZipEntry dosOnly = new ZipEntry(DOS_ONLY);
            dosOnly.setTime(DOS_ONLY_TIME.toEpochMilli());
            jar.putNextEntry(dosOnly);
            jar.write(("content-of-" + DOS_ONLY).getBytes("UTF-8"));
            jar.closeEntry();
        }
        return archive;
    }

    /** Stage 3: the written times survive a JarFile / ZipFile read-back. */
    static void entryTimesRoundTrip(File archive) throws IOException {
        try (JarFile jar = new JarFile(archive)) {
            Enumeration<JarEntry> entries = jar.entries();
            while (entries.hasMoreElements()) {
                JarEntry entry = entries.nextElement();
                emit("jarentry." + entry.getName(), entry.getLastModifiedTime().toInstant());
                emit("jarentry.getTime." + entry.getName(), Instant.ofEpochMilli(entry.getTime()));
                // Null on HotSpot for every one of these: the central
                // directory carries the modified time alone.
                emit("jarentry.access." + entry.getName(), entry.getLastAccessTime());
                emit("jarentry.creation." + entry.getName(), entry.getCreationTime());
            }
            // The DOS-only entry has no mtime, so this is the one read that
            // reaches `xdostime` -- the field the loader bridge was
            // overwriting with the entry's size.
            JarEntry dosOnly = jar.getJarEntry(DOS_ONLY);
            emit("jarentry.dosOnly.getJarEntry", dosOnly.getLastModifiedTime().toInstant());
        }
        try (ZipFile zip = new ZipFile(archive)) {
            for (String name : NAMES) {
                ZipEntry entry = zip.getEntry(name);
                emit("zipentry." + name, entry.getLastModifiedTime().toInstant());
                emit("zipentry.access." + name, entry.getLastAccessTime());
                emit("zipentry.creation." + name, entry.getCreationTime());
            }
            ZipEntry dosOnly = zip.getEntry(DOS_ONLY);
            emit("zipentry.dosOnly", dosOnly.getLastModifiedTime().toInstant());
            emit("zipentry.dosOnly.getTime", Instant.ofEpochMilli(dosOnly.getTime()));
        }
    }

    /** Stage 4: the whole ExtractCommand step -- extract, setTimes, read back. */
    static void extractedFileTimes(File dir, File archive) throws IOException {
        File out = new File(dir, "extracted");
        if (!out.mkdirs()) {
            throw new IOException("cannot create " + out);
        }
        try (JarFile jar = new JarFile(archive)) {
            Enumeration<JarEntry> entries = jar.entries();
            while (entries.hasMoreElements()) {
                JarEntry entry = entries.nextElement();
                File target = new File(out, entry.getName().replace('/', '_'));
                try (InputStream in = jar.getInputStream(entry);
                        OutputStream sink = new FileOutputStream(target)) {
                    byte[] buf = new byte[4096];
                    int read;
                    while ((read = in.read(buf)) > 0) {
                        sink.write(buf, 0, read);
                    }
                }
                FileTime modified = entry.getLastModifiedTime();
                Files.getFileAttributeView(target.toPath(), BasicFileAttributeView.class)
                    .setTimes(modified, modified, modified);
                BasicFileAttributes attrs = Files
                    .getFileAttributeView(target.toPath(), BasicFileAttributeView.class)
                    .readAttributes();
                emit("extracted." + target.getName(), attrs.lastModifiedTime().toInstant());
            }
        }
    }

    static void emit(String what, Object value) {
        checks++;
        System.out.println("CK " + what + " " + value);
    }

    static void deleteTree(File file) {
        File[] children = file.listFiles();
        if (children != null) {
            for (File child : children) {
                deleteTree(child);
            }
        }
        if (!file.delete()) {
            // Best effort: a leftover temp tree must not fail the vector.
            file.deleteOnExit();
        }
    }

    private RFileTimes() {
    }
}
