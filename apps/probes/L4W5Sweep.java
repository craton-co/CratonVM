// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayInputStream;
import java.io.File;
import java.io.FileDescriptor;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.RandomAccessFile;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.channels.SeekableByteChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.OpenOption;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.nio.file.attribute.BasicFileAttributeView;
import java.nio.file.attribute.BasicFileAttributes;
import java.nio.file.attribute.FileTime;
import java.time.Instant;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;
import java.util.concurrent.TimeUnit;

/**
 * Lane 4 wave 5 — the four families the wave-1 builds backed out.
 *
 * One probe over all four, because the screen that decides the wave is the
 * corpus and this tree's job is the FUNNEL: every row the table retires has to
 * be shown invoked, with `invocations > 0` in a census taken from this run.
 *
 * Everything printed is DETERMINISTIC by construction. Timestamps are fixed
 * constants, never a file's own clock; sizes are of content written here; and
 * every path is scrubbed to `<tmp>/...` before it is printed, because the
 * temp directory's name differs per run and a diff that reports 261 rows
 * differing has told you nothing. Where the JDK's own answer is
 * platform-dependent (`creationTime` on Linux) the row prints a SHAPE rather
 * than the value.
 */
public class L4W5Sweep {

    static String tmpRoot = "";

    static void p(String k, Object v) {
        System.out.println(k + " = " + scrub(String.valueOf(v)));
    }

    static String scrub(String s) {
        if (s == null) {
            return "null";
        }
        if (!tmpRoot.isEmpty()) {
            s = s.replace(tmpRoot, "<tmp>");
        }
        return s;
    }

    static String cls(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static String bytes(byte[] b, int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) {
            sb.append(Integer.toHexString(b[i] & 0xff));
            sb.append(' ');
        }
        return sb.toString().trim();
    }

    // Fixed, whole-second instants: the filesystems this runs on disagree below
    // a second, and this probe is diffed byte for byte.
    static final long T_MOD = 1609459200000L; // 2021-01-01T00:00:00Z
    static final long T_ACC = 1640995200000L; // 2022-01-01T00:00:00Z
    static final long T_CRE = 1577836800000L; // 2020-01-01T00:00:00Z

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l4w5");
        tmpRoot = dir.toString();
        try {
            fileTimeValues();
            attributes(dir);
            posix(dir);
            byteArrayInput();
            fileHandles(dir);
            provider(dir);
        } finally {
            System.out.println("-- done");
        }
    }

    // ---------------------------------------------------------------- FileTime

    static void fileTimeValues() {
        System.out.println("-- filetime");
        FileTime a = FileTime.fromMillis(T_MOD);
        p("fromMillis.toMillis", a.toMillis());
        p("fromMillis.toString", a.toString());
        p("fromMillis.cls", cls(a));
        p("fromMillis.toInstant", a.toInstant());
        p("fromMillis.to.SECONDS", a.to(TimeUnit.SECONDS));
        p("fromMillis.to.MILLIS", a.to(TimeUnit.MILLISECONDS));
        p("fromMillis.to.NANOS", a.to(TimeUnit.NANOSECONDS));
        p("fromMillis.to.DAYS", a.to(TimeUnit.DAYS));

        FileTime b = FileTime.from(T_MOD / 1000L, TimeUnit.SECONDS);
        p("fromSeconds.toMillis", b.toMillis());
        p("fromSeconds.toString", b.toString());
        p("fromSeconds.equalsMillis", b.equals(a));
        p("fromSeconds.compareTo", b.compareTo(a));
        p("fromSeconds.hashEq", b.hashCode() == a.hashCode());

        FileTime c = FileTime.from(Instant.ofEpochMilli(T_ACC));
        p("fromInstant.toMillis", c.toMillis());
        p("fromInstant.toString", c.toString());
        p("fromInstant.toInstant", c.toInstant());
        p("fromInstant.compareTo", c.compareTo(a));
        p("fromInstant.compareBack", a.compareTo(c));

        FileTime zero = FileTime.fromMillis(0L);
        p("epoch.toMillis", zero.toMillis());
        p("epoch.toString", zero.toString());
        p("epoch.equalsSelf", zero.equals(FileTime.fromMillis(0L)));
        p("epoch.equalsOther", zero.equals("x"));

        FileTime neg = FileTime.fromMillis(-1000L);
        p("neg.toMillis", neg.toMillis());
        p("neg.toString", neg.toString());
        p("neg.compareTo", neg.compareTo(zero));

        // A unit that cannot be represented in millis without loss.
        FileTime nanos = FileTime.from(1609459200123456789L, TimeUnit.NANOSECONDS);
        p("nanos.toMillis", nanos.toMillis());
        p("nanos.toString", nanos.toString());
        p("nanos.to.NANOS", nanos.to(TimeUnit.NANOSECONDS));

        // Sorting is compareTo through a real consumer.
        FileTime[] all = {c, a, zero, neg};
        Arrays.sort(all);
        StringBuilder sb = new StringBuilder();
        for (FileTime f : all) {
            sb.append(f.toMillis()).append(' ');
        }
        p("sorted", sb.toString().trim());

        Set<FileTime> set = new HashSet<>();
        set.add(a);
        set.add(FileTime.fromMillis(T_MOD));
        p("hashset.size", set.size());
    }

    // -------------------------------------------------------------- attributes

    static void attributes(Path dir) throws Exception {
        System.out.println("-- attributes");
        Path f = dir.resolve("attrs.txt");
        Files.write(f, "0123456789".getBytes(StandardCharsets.UTF_8));

        // The SETTER first: §9.2 names it as the cause of RFileTimes, so it is
        // the row this section exists to exercise.
        Files.setLastModifiedTime(f, FileTime.fromMillis(T_MOD));
        p("setLastModified.read", Files.getLastModifiedTime(f).toMillis());
        p("setLastModified.cls", cls(Files.getLastModifiedTime(f)));

        BasicFileAttributeView view =
                Files.getFileAttributeView(f, BasicFileAttributeView.class);
        p("view.cls", cls(view));
        p("view.name", view.name());

        view.setTimes(FileTime.fromMillis(T_MOD), FileTime.fromMillis(T_ACC),
                FileTime.fromMillis(T_CRE));
        BasicFileAttributes at = view.readAttributes();
        p("view.attrs.cls", cls(at));
        p("view.lastModified", at.lastModifiedTime().toMillis());
        p("view.lastAccess", at.lastAccessTime().toMillis());
        // Creation time is settable on Windows and ignored by the Linux kernel,
        // so HotSpot itself answers differently per host: print a shape.
        p("view.creation.nonNull", at.creationTime() != null);
        p("view.size", at.size());
        p("view.isRegularFile", at.isRegularFile());
        p("view.isDirectory", at.isDirectory());
        p("view.isSymbolicLink", at.isSymbolicLink());
        p("view.isOther", at.isOther());
        p("view.fileKey.nonNull", at.fileKey() != null);

        BasicFileAttributes at2 = Files.readAttributes(f, BasicFileAttributes.class);
        p("read.lastModified", at2.lastModifiedTime().toMillis());
        p("read.size", at2.size());
        p("read.sameTimes", at2.lastModifiedTime().equals(at.lastModifiedTime()));

        BasicFileAttributes atNo = Files.readAttributes(f, BasicFileAttributes.class,
                LinkOption.NOFOLLOW_LINKS);
        p("readNoFollow.lastModified", atNo.lastModifiedTime().toMillis());

        // setTimes with nulls must leave the other stamps alone.
        view.setTimes(null, null, null);
        p("setTimes.allNull.lastModified", view.readAttributes().lastModifiedTime().toMillis());
        view.setTimes(FileTime.fromMillis(T_ACC), null, null);
        p("setTimes.modOnly.lastModified", view.readAttributes().lastModifiedTime().toMillis());
        p("setTimes.modOnly.lastAccess", view.readAttributes().lastAccessTime().toMillis());

        Path d = dir.resolve("subdir");
        Files.createDirectory(d);
        BasicFileAttributes da = Files.readAttributes(d, BasicFileAttributes.class);
        p("dir.isDirectory", da.isDirectory());
        p("dir.isRegularFile", da.isRegularFile());

        p("files.getAttribute.size", Files.getAttribute(f, "basic:size"));
        p("files.getAttribute.mtime", ((FileTime) Files.getAttribute(f, "basic:lastModifiedTime")).toMillis());
        Files.setAttribute(f, "basic:lastModifiedTime", FileTime.fromMillis(T_MOD));
        p("files.setAttribute.read", Files.getLastModifiedTime(f).toMillis());
    }

    // ------------------------------------------------------------------- posix

    /**
     * `PosixFilePermission` and `PosixFilePermissions` are the rest of
     * `java/nio/file/attribute/`. Nothing above reaches them, and the funnel
     * rule is that a row no probe invokes must not be taken — so they get
     * their own section rather than a footnote saying they are probably fine.
     */
    static void posix(Path dir) throws Exception {
        System.out.println("-- posix");
        java.nio.file.attribute.PosixFilePermission[] all =
                java.nio.file.attribute.PosixFilePermission.values();
        p("perm.values.len", all.length);
        StringBuilder names = new StringBuilder();
        for (java.nio.file.attribute.PosixFilePermission x : all) {
            names.append(x.name()).append(':').append(x.ordinal()).append(' ');
        }
        p("perm.values", names.toString().trim());
        java.nio.file.attribute.PosixFilePermission one =
                java.nio.file.attribute.PosixFilePermission.valueOf("OWNER_READ");
        p("perm.valueOf", one);
        p("perm.valueOf.ordinal", one.ordinal());
        p("perm.valueOf.same", one == java.nio.file.attribute.PosixFilePermission.OWNER_READ);
        try {
            java.nio.file.attribute.PosixFilePermission.valueOf("NOT_A_PERMISSION");
            System.out.println("perm.valueOf.bad = no throw");
        } catch (Throwable e) {
            System.out.println("perm.valueOf.bad = " + e.getClass().getName());
        }

        Set<java.nio.file.attribute.PosixFilePermission> parsed =
                java.nio.file.attribute.PosixFilePermissions.fromString("rwxr-x---");
        p("perms.fromString.size", parsed.size());
        p("perms.fromString.round", java.nio.file.attribute.PosixFilePermissions.toString(parsed));
        Set<java.nio.file.attribute.PosixFilePermission> none =
                java.nio.file.attribute.PosixFilePermissions.fromString("---------");
        p("perms.empty.size", none.size());
        p("perms.empty.round", java.nio.file.attribute.PosixFilePermissions.toString(none));
        p("perms.all.round", java.nio.file.attribute.PosixFilePermissions.toString(
                java.nio.file.attribute.PosixFilePermissions.fromString("rwxrwxrwx")));
        try {
            java.nio.file.attribute.PosixFilePermissions.fromString("rwx");
            System.out.println("perms.fromString.short = no throw");
        } catch (Throwable e) {
            System.out.println("perms.fromString.short = " + e.getClass().getName());
        }

        java.nio.file.attribute.FileAttribute<?> fa =
                java.nio.file.attribute.PosixFilePermissions.asFileAttribute(parsed);
        p("perms.asFileAttribute.name", fa.name());
        p("perms.asFileAttribute.valueSize",
                ((Set<?>) fa.value()).size());

        Path pf = dir.resolve("posix.txt");
        Files.write(pf, new byte[] {1});
        try {
            Files.setPosixFilePermissions(pf, parsed);
            p("files.setPosix.read", java.nio.file.attribute.PosixFilePermissions.toString(
                    Files.getPosixFilePermissions(pf)));
        } catch (Throwable e) {
            System.out.println("files.setPosix.read = " + e.getClass().getName());
        }
    }

    // ------------------------------------------------------- ByteArrayInputStream

    static void byteArrayInput() throws Exception {
        System.out.println("-- bais");
        byte[] data = new byte[16];
        for (int i = 0; i < data.length; i++) {
            data[i] = (byte) (i * 7);
        }

        ByteArrayInputStream in = new ByteArrayInputStream(data);
        p("bais.cls", cls(in));
        p("bais.available0", in.available());
        p("bais.markSupported", in.markSupported());
        p("bais.read1", in.read());
        p("bais.read2", in.read());
        p("bais.available2", in.available());

        byte[] buf = new byte[6];
        // The one-argument `read(byte[])` is INHERITED from InputStream (bucket
        // B), and it is a different registration from `read(byte[],int,int)`.
        byte[] two = new byte[2];
        p("bais.readArr1", in.read(two));
        p("bais.readArr1.bytes", bytes(two, 2));
        p("bais.readArr", in.read(buf, 0, 6));
        p("bais.readArr.bytes", bytes(buf, 6));
        p("bais.availableMid", in.available());

        in.mark(0);
        p("bais.skip3", in.skip(3));
        p("bais.afterSkip", in.read());
        in.reset();
        p("bais.afterReset", in.read());

        p("bais.skipPastEnd", in.skip(1000L));
        p("bais.readAtEnd", in.read());
        p("bais.availableEnd", in.available());
        p("bais.readArrAtEnd", in.read(buf, 0, 6));
        in.close();
        p("bais.readAfterClose", in.read());

        ByteArrayInputStream off = new ByteArrayInputStream(data, 4, 5);
        p("offset.available", off.available());
        p("offset.read", off.read());
        byte[] rest = off.readAllBytes();
        p("offset.readAllBytes.len", rest.length);
        p("offset.readAllBytes", bytes(rest, rest.length));

        ByteArrayInputStream n = new ByteArrayInputStream(data);
        byte[] four = n.readNBytes(4);
        p("readNBytes.len", four.length);
        p("readNBytes", bytes(four, four.length));
        p("readNBytes.into", n.readNBytes(buf, 0, 4));
        p("readNBytes.into.bytes", bytes(buf, 4));

        ByteArrayInputStream tr = new ByteArrayInputStream(data);
        java.io.ByteArrayOutputStream sink = new java.io.ByteArrayOutputStream();
        p("transferTo", tr.transferTo(sink));
        p("transferTo.sink", bytes(sink.toByteArray(), sink.size()));

        // Through a plain InputStream reference, so the call site is not the
        // concrete class: a native claimed by the declaring class answers here
        // too, and a lambda-free local is what makes the counter see it.
        InputStream via = new ByteArrayInputStream(data, 0, 3);
        p("viaInputStream.available", via.available());
        p("viaInputStream.read", via.read());

        // Plainly, not through a lambda: a call made inside one has already
        // been measured on this lane answering correctly with `invocations: 0`,
        // and a funnel that cannot show a row invoked must not take that row.
        ByteArrayInputStream bad = new ByteArrayInputStream(data);
        try {
            bad.read(new byte[2], 0, 5);
            System.out.println("bais.negativeLen = no throw");
        } catch (Throwable e) {
            System.out.println("bais.negativeLen = " + e.getClass().getName());
        }
        try {
            bad.read(null, 0, 1);
            System.out.println("bais.nullBuf = no throw");
        } catch (Throwable e) {
            System.out.println("bais.nullBuf = " + e.getClass().getName());
        }
    }

    // ------------------------------------------------------------- file handles

    static void fileHandles(Path dir) throws Exception {
        System.out.println("-- handles");
        File f = dir.resolve("handle.bin").toFile();

        FileOutputStream out = new FileOutputStream(f);
        p("fos.cls", cls(out));
        out.write(65);
        out.write(new byte[] {66, 67, 68});
        out.write(new byte[] {69, 70, 71, 72}, 1, 2);
        FileDescriptor fd = out.getFD();
        p("fos.fd.cls", cls(fd));
        p("fos.fd.valid", fd.valid());
        fd.sync();
        FileChannel ch = out.getChannel();
        p("fos.channel.cls", cls(ch));
        p("fos.channel.position", ch.position());
        p("fos.channel.size", ch.size());
        p("fos.channel.isOpen", ch.isOpen());
        out.flush();
        out.close();
        p("fos.fd.validAfterClose", fd.valid());
        p("fos.channel.isOpenAfterClose", ch.isOpen());
        p("file.length", f.length());

        FileInputStream in = new FileInputStream(f);
        p("fis.available", in.available());
        p("fis.fd.valid", in.getFD().valid());
        FileChannel rc = in.getChannel();
        p("fis.channel.size", rc.size());
        ByteBuffer bb = ByteBuffer.allocate(4);
        p("fis.channel.read", rc.read(bb));
        p("fis.channel.read.bytes", bytes(bb.array(), 4));
        p("fis.channel.position", rc.position());
        rc.position(1L);
        p("fis.channel.reposition", rc.position());
        bb.clear();
        p("fis.channel.readAt", rc.read(bb, 0L));
        p("fis.channel.readAt.bytes", bytes(bb.array(), 4));
        in.close();
        p("fis.channel.isOpenAfterClose", rc.isOpen());

        // close() on the CHANNEL rather than on the stream: a different
        // registration from the stream's, and the only caller that reaches it.
        FileOutputStream c2 = new FileOutputStream(dir.resolve("chan.bin").toFile());
        FileChannel och = c2.getChannel();
        p("chan.isOpenBefore", och.isOpen());
        och.close();
        p("chan.isOpenAfter", och.isOpen());
        c2.close();

        FileOutputStream app = new FileOutputStream(f, true);
        app.write(new byte[] {90});
        app.close();
        p("append.length", f.length());

        // FileChannel as a SeekableByteChannel, through the interface.
        try (SeekableByteChannel sc = Files.newByteChannel(f.toPath(),
                StandardOpenOption.READ)) {
            p("sbc.cls", cls(sc));
            p("sbc.size", sc.size());
            ByteBuffer one = ByteBuffer.allocate(2);
            p("sbc.read", sc.read(one));
            p("sbc.position", sc.position());
        }

        try (FileChannel w = FileChannel.open(f.toPath(), StandardOpenOption.WRITE)) {
            p("open.size", w.size());
            p("open.truncate", w.truncate(3L).size());
            w.force(true);
            p("open.positionAfterTruncate", w.position());
            ByteBuffer src = ByteBuffer.wrap(new byte[] {33, 34});
            p("open.write", w.write(src));
            p("open.sizeAfterWrite", w.size());
        }
        p("file.lengthAfter", f.length());

        try (RandomAccessFile raf = new RandomAccessFile(f, "rw")) {
            p("raf.fd.valid", raf.getFD().valid());
            p("raf.length", raf.length());
            FileChannel rch = raf.getChannel();
            p("raf.channel.size", rch.size());
            p("raf.channel.cls", cls(rch));
        }

        p("fd.in.valid", FileDescriptor.in.valid());
        p("fd.out.valid", FileDescriptor.out.valid());
        p("fd.err.valid", FileDescriptor.err.valid());

        // A stream dropped without close(): FileCleanable is the registered
        // cleanup, and nothing but a real collection reaches it.
        FileOutputStream leak = new FileOutputStream(dir.resolve("leak.bin").toFile());
        leak.write(1);
        leak = null;
        System.gc();
        Thread.sleep(50L);
        p("cleanable.fileExists", dir.resolve("leak.bin").toFile().exists());
    }

    // ------------------------------------------------------------ provider

    static void provider(Path dir) throws Exception {
        System.out.println("-- provider");
        FileSystem fs = FileSystems.getDefault();
        p("fs.cls", cls(fs));
        p("provider.cls", cls(fs.provider()));
        p("provider.scheme", fs.provider().getScheme());
        p("fs.isOpen", fs.isOpen());
        p("fs.isReadOnly", fs.isReadOnly());
        p("fs.separator", fs.getSeparator());

        Path src = dir.resolve("p-src.txt");
        Files.write(src, "hello".getBytes(StandardCharsets.UTF_8));
        p("exists", Files.exists(src));
        p("size", Files.size(src));
        p("isRegularFile", Files.isRegularFile(src));
        p("isReadable", Files.isReadable(src));
        p("isWritable", Files.isWritable(src));

        Path copy = dir.resolve("p-copy.txt");
        Files.copy(src, copy, StandardCopyOption.REPLACE_EXISTING);
        p("copy.size", Files.size(copy));
        p("copy.sameFile", Files.isSameFile(src, copy));
        p("copy.sameFileSelf", Files.isSameFile(src, src));

        Path moved = dir.resolve("p-moved.txt");
        Files.move(copy, moved, StandardCopyOption.REPLACE_EXISTING);
        p("move.exists", Files.exists(moved));
        p("move.oldGone", !Files.exists(copy));

        Path link = dir.resolve("p-link.txt");
        try {
            Files.createLink(link, src);
            System.out.println("createLink = ok");
        } catch (Throwable e) {
            System.out.println("createLink = " + e.getClass().getName());
        }
        p("link.exists", Files.exists(link));
        p("link.size", Files.exists(link) ? Files.size(link) : -1L);

        Path sym = dir.resolve("p-sym.txt");
        try {
            Files.createSymbolicLink(sym, src);
            System.out.println("createSymbolicLink = ok");
        } catch (Throwable e) {
            System.out.println("createSymbolicLink = " + e.getClass().getName());
        }
        p("sym.isSymbolicLink", Files.isSymbolicLink(sym));
        if (Files.isSymbolicLink(sym)) {
            p("sym.readTarget", Files.readSymbolicLink(sym).getFileName());
            p("sym.followedSize", Files.size(sym));
        }

        try (InputStream is = Files.newInputStream(src)) {
            p("newInputStream.cls", cls(is));
            p("newInputStream.read", is.read());
        }
        Path outp = dir.resolve("p-out.txt");
        try (java.io.OutputStream os = Files.newOutputStream(outp,
                StandardOpenOption.CREATE, StandardOpenOption.WRITE)) {
            p("newOutputStream.cls", cls(os));
            os.write(new byte[] {1, 2, 3});
        }
        p("newOutputStream.size", Files.size(outp));

        Files.delete(moved);
        p("delete.gone", !Files.exists(moved));
        p("deleteIfExists.false", Files.deleteIfExists(moved));

        try {
            Files.delete(dir.resolve("never-existed.txt"));
            System.out.println("delete.missing = no throw");
        } catch (Throwable e) {
            System.out.println("delete.missing = " + e.getClass().getName());
        }

        p("newDirectoryStream.count", count(dir));
    }

    static int count(Path dir) throws IOException {
        int n = 0;
        try (java.nio.file.DirectoryStream<Path> ds = Files.newDirectoryStream(dir)) {
            for (Path ignored : ds) {
                n++;
            }
        }
        return n;
    }
}
