// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.IOException;
import java.lang.reflect.Method;
import java.net.URI;
import java.nio.ByteBuffer;
import java.nio.channels.SeekableByteChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.DirectoryStream;
import java.nio.file.FileStore;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.nio.file.StandardWatchEventKinds;
import java.nio.file.WatchEvent;
import java.nio.file.WatchKey;
import java.nio.file.WatchService;
import java.nio.file.attribute.AclFileAttributeView;
import java.nio.file.attribute.BasicFileAttributeView;
import java.nio.file.attribute.BasicFileAttributes;
import java.nio.file.attribute.DosFileAttributeView;
import java.nio.file.attribute.FileOwnerAttributeView;
import java.nio.file.attribute.FileStoreAttributeView;
import java.nio.file.attribute.PosixFileAttributeView;
import java.nio.file.attribute.UserDefinedFileAttributeView;
import java.nio.file.attribute.FileTime;
import java.nio.file.attribute.PosixFileAttributes;
import java.nio.file.attribute.PosixFilePermissions;
import java.nio.file.spi.FileSystemProvider;
import java.time.Instant;
import java.util.ArrayList;
import java.util.Collections;
import java.util.EnumSet;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.TimeUnit;
import java.util.stream.Collectors;
import java.util.stream.Stream;

/**
 * SBR wave 2, lane {@code fs}: the retired Linux {@code sun/nio/fs} surface --
 * {@code UnixPath}, {@code UnixFileAttributes}, {@code LinuxFileSystemProvider}
 * and the directory streams -- plus the genuine dispatcher natives the real
 * bytecode reaches underneath ({@code opendir0}/{@code readdir0},
 * {@code futimens0}, {@code directCopy0}, ...). Wave 5 (lane {@code fs2}) added the
 * rows wave 2 held: the Linux {@code WatchService} pipeline, {@code FileStore} and the
 * {@code DefaultFileSystemProvider} doors (run every arm with
 * {@code --add-opens java.base/sun.nio.fs=ALL-UNNAMED}; the last row calls them reflectively).
 *
 * <p>Every row is evaluated ONCE and printed (a native that wins from the first
 * call shows here), then re-evaluated inside a hot loop and compared with that
 * first value; {@code drift} counts mismatches, so cold == warm is asserted.
 * File rows are self-contained (create, read back, delete), so they are
 * idempotent across iterations. Paths are printed relative to the scratch base,
 * so the output is comparable across VMs. Diff the whole output against
 * HotSpot 25.
 *
 * <pre>
 *   java SbrW2Fs                        (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW2Fs
 * </pre>
 */
public class SbrW2Fs {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Sup> ROWS = new ArrayList<>();
    static Path BASE;

    interface Sup {
        String get() throws Exception;
    }

    static void row(String label, Sup s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    static String safe(Sup s) {
        try {
            return s.get();
        } catch (Throwable t) {
            String m = t.getMessage();
            if (m != null && BASE != null) m = m.replace(BASE.toString(), "$BASE");
            return "!" + t.getClass().getName() + ": " + m;
        }
    }

    static String rel(Path p) {
        if (p == null) return "null";
        return p.startsWith(BASE) ? "$BASE/" + BASE.relativize(p) : p.toString();
    }

    static String units(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }

    static Path fresh(String name) throws IOException {
        Path d = BASE.resolve(name);
        rm(d);
        Files.createDirectory(d);
        return d;
    }

    static void rm(Path p) throws IOException {
        if (!Files.exists(p, LinkOption.NOFOLLOW_LINKS)) return;
        if (Files.isDirectory(p, LinkOption.NOFOLLOW_LINKS)) {
            try (Stream<Path> s = Files.list(p)) {
                for (Path c : s.collect(Collectors.toList())) rm(c);
            }
        }
        Files.delete(p);
    }

    /** A key's pending events, sorted, as {@code kind:context:count}. */
    static String events(WatchKey k) {
        if (k == null) return "null";
        List<String> out = new ArrayList<>();
        for (WatchEvent<?> e : k.pollEvents()) {
            out.add(e.kind().name() + ":" + e.context() + ":" + e.count()
                    + (e.kind() == StandardWatchEventKinds.ENTRY_CREATE ? ":C" : "")
                    + (e.context() instanceof Path ? "" : ":!path"));
        }
        Collections.sort(out);
        return out.toString();
    }

    static String pathShape(String in) {
        Path p = Path.of(in);
        Path other = Path.of("a/b");
        StringBuilder b = new StringBuilder();
        b.append("str=").append(p)
         .append(" cls=").append(p.getClass().getName())
         .append(" file=").append(p.getFileName())
         .append(" parent=").append(p.getParent())
         .append(" root=").append(p.getRoot())
         .append(" n=").append(p.getNameCount())
         .append(" abs=").append(p.isAbsolute())
         .append(" norm=").append(p.normalize())
         .append(" hash=").append(p.hashCode())
         .append(" eqSelf=").append(p.equals(Path.of(in)))
         .append(" cmp=").append(Integer.signum(p.compareTo(other)))
         .append(" resolveZ=").append(p.resolve("z"))
         .append(" resolveAbs=").append(p.resolve(Path.of("/abs")))
         .append(" startsA=").append(p.startsWith(Path.of("a")))
         .append(" endsC=").append(p.endsWith(Path.of("c")))
         .append(" fsIsDefault=").append(p.getFileSystem() == FileSystems.getDefault());
        if (p.getNameCount() > 0 && !p.toString().isEmpty()) {
            b.append(" name0=").append(p.getName(0))
             .append(" sub=").append(p.subpath(0, p.getNameCount()));
        }
        List<String> it = new ArrayList<>();
        for (Path e : p) it.add(e.toString());
        b.append(" iter=").append(it);
        try {
            b.append(" relFromAB=").append(other.relativize(p));
        } catch (IllegalArgumentException e) {
            b.append(" relFromAB=!IAE");
        }
        return b.toString();
    }

    public static void main(String[] args) throws Exception {
        BASE = Path.of("sbrw2fs.d").toAbsolutePath();
        rm(BASE);
        Files.createDirectory(BASE);
        Path cwd = Path.of("").toAbsolutePath();

        for (String in : new String[] {"/a/b/../c/./d", "a//b/", "/", "", "..", "a/b/c",
                                       "/tmp/x.txt", "é/ü", "a/./b/../../c", "//x//y"}) {
            row("path[" + in + "]", () -> pathShape(in));
        }
        row("toAbsolutePath", () -> {
            Path a = Path.of("q/r").toAbsolutePath();
            return "abs=" + a.isAbsolute() + " underCwd=" + a.startsWith(cwd)
                    + " tail=" + cwd.relativize(a) + " absOfAbs=" + (a.toAbsolutePath() == a);
        });
        row("relativize", () -> Path.of("/a/b/c").relativize(Path.of("/a/x/y")) + " "
                + Path.of("/a").relativize(Path.of("/a")) + " "
                + Path.of("a/b").relativize(Path.of("a/b/c/d")));
        row("equalsAcross", () -> {
            Path x = Path.of("/p/q"), y = FileSystems.getDefault().getPath("/p", "q");
            return x.equals(y) + " " + (x.hashCode() == y.hashCode()) + " " + x.compareTo(y)
                    + " " + x.equals("/p/q") + " " + Path.of("/p/q/").equals(x);
        });
        row("toUri", () -> {
            Path d = fresh("uri");
            Path f = Files.createFile(d.resolve("f x.txt"));
            String r = BASE.toUri().relativize(d.toUri()) + " | " + BASE.toUri().relativize(f.toUri())
                    + " | " + Path.of(f.toUri()).equals(f) + " | " + d.toUri().getScheme();
            rm(d);
            return r;
        });
        row("toRealPath", () -> {
            Path d = fresh("real");
            Path f = Files.createFile(d.resolve("t"));
            Path l = Files.createSymbolicLink(d.resolve("l"), f.getFileName());
            String r = rel(d.resolve("./l").toRealPath()) + " "
                    + rel(d.resolve("x/../l").toRealPath(LinkOption.NOFOLLOW_LINKS)) + " "
                    + safe(() -> rel(d.resolve("missing").toRealPath()));
            rm(d);
            return r;
        });
        row("provider", () -> {
            FileSystem fs = FileSystems.getDefault();
            FileSystemProvider p = fs.provider();
            return p.getClass().getName() + " scheme=" + p.getScheme()
                    + " byUri=" + (p.getFileSystem(URI.create("file:///")) == fs)
                    + " path=" + p.getPath(URI.create("file:///a/b%20c"))
                    + " installed0=" + (FileSystemProvider.installedProviders().get(0) == p)
                    + " newFs=" + safe(() -> "" + p.newFileSystem(URI.create("file:///"), Map.of()))
                    + " newFsPath=" + safe(() -> "" + p.newFileSystem(Path.of("/x.zip"), Map.of()));
        });
        row("attrs.basic", () -> {
            Path d = fresh("attrs");
            Path f = Files.writeString(d.resolve("f"), "hello");
            FileTime mt = FileTime.from(Instant.parse("2021-01-01T00:00:00.123456789Z"));
            FileTime at = FileTime.from(Instant.parse("2022-02-02T02:02:02.5Z"));
            Files.getFileAttributeView(f, BasicFileAttributeView.class).setTimes(mt, at, null);
            BasicFileAttributes a = Files.readAttributes(f, BasicFileAttributes.class);
            String r = "size=" + a.size() + " reg=" + a.isRegularFile() + " dir=" + a.isDirectory()
                    + " link=" + a.isSymbolicLink() + " other=" + a.isOther()
                    + " mtime=" + a.lastModifiedTime() + " atime=" + a.lastAccessTime()
                    + " key=" + (a.fileKey() != null) + " keyEq="
                    + a.fileKey().equals(Files.readAttributes(f, BasicFileAttributes.class).fileKey())
                    + " posixCast=" + (a instanceof PosixFileAttributes)
                    + " birthRecent=" + (a.creationTime().toMillis() > System.currentTimeMillis() - 3_600_000L)
                    + " getLMT=" + Files.getLastModifiedTime(f)
                    + " sizeF=" + Files.size(f);
            rm(d);
            return r;
        });
        row("attrs.setLastModified", () -> {
            Path d = fresh("slm");
            Path f = Files.createFile(d.resolve("f"));
            Files.setLastModifiedTime(f, FileTime.fromMillis(1_609_459_200_000L));
            String r = Files.getLastModifiedTime(f) + " "
                    + Files.getAttribute(f, "basic:lastModifiedTime");
            rm(d);
            return r;
        });
        row("attrs.dirLinkOther", () -> {
            Path d = fresh("dlo");
            Path l = Files.createSymbolicLink(d.resolve("l"), Path.of("nowhere"));
            BasicFileAttributes da = Files.readAttributes(d, BasicFileAttributes.class);
            BasicFileAttributes la = Files.readAttributes(l, BasicFileAttributes.class, LinkOption.NOFOLLOW_LINKS);
            BasicFileAttributes na = Files.readAttributes(Path.of("/dev/null"), BasicFileAttributes.class);
            String r = "dir=" + da.isDirectory() + "/" + da.isRegularFile() + " link=" + la.isSymbolicLink()
                    + "/" + la.isRegularFile() + "/" + la.isOther() + " devnull.other=" + na.isOther()
                    + "/" + na.isRegularFile() + "/" + na.size()
                    + " missing=" + safe(() -> "" + Files.readAttributes(d.resolve("nope"), BasicFileAttributes.class));
            rm(d);
            return r;
        });
        row("attrs.map", () -> {
            Path d = fresh("amap");
            Path f = Files.writeString(d.resolve("f"), "abc");
            Files.setPosixFilePermissions(f, PosixFilePermissions.fromString("rw-r-----"));
            Map<String, Object> m = new TreeMap<>(Files.readAttributes(f, "posix:permissions,size,isDirectory"));
            String r = m.keySet() + " size=" + m.get("size") + " dir=" + m.get("isDirectory")
                    + " perms=" + PosixFilePermissions.toString(Files.getPosixFilePermissions(f))
                    + " owner=" + (Files.getOwner(f) != null);
            rm(d);
            return r;
        });
        row("access", () -> {
            Path d = fresh("acc");
            Path f = Files.createFile(d.resolve("f"));
            Files.setPosixFilePermissions(f, PosixFilePermissions.fromString("r--------"));
            String r = Files.isReadable(f) + " " + Files.isExecutable(f) + " " + Files.exists(f)
                    + " " + Files.isHidden(d.resolve(".h")) + " " + Files.isHidden(f)
                    + " same=" + Files.isSameFile(f, d.resolve("./f"))
                    + " missing=" + safe(() -> { f.getFileSystem().provider().checkAccess(d.resolve("x")); return "ok"; });
            rm(d);
            return r;
        });
        row("dirStream", () -> {
            Path d = fresh("ds");
            for (String n : new String[] {"b.txt", "a.txt", "c.log", ".hid"}) Files.createFile(d.resolve(n));
            Files.createDirectory(d.resolve("sub"));
            List<String> all = new ArrayList<>(), txt = new ArrayList<>();
            try (DirectoryStream<Path> s = Files.newDirectoryStream(d)) {
                for (Path p : s) all.add(rel(p));
            }
            try (DirectoryStream<Path> s = Files.newDirectoryStream(d, "*.txt")) {
                for (Path p : s) txt.add(p.getFileName().toString());
            }
            Collections.sort(all);
            Collections.sort(txt);
            String cls;
            try (DirectoryStream<Path> s = Files.newDirectoryStream(d)) {
                cls = s.getClass().getName();
            }
            String twice = safe(() -> {
                try (DirectoryStream<Path> s = Files.newDirectoryStream(d)) {
                    s.iterator();
                    s.iterator();
                }
                return "no";
            });
            String notDir = safe(() -> { Files.newDirectoryStream(d.resolve("a.txt")).close(); return "no"; });
            String r = all + " " + txt + " " + cls + " twice=" + twice + " notDir=" + notDir;
            rm(d);
            return r;
        });
        row("walk", () -> {
            Path d = fresh("walk");
            Files.createDirectories(d.resolve("x/y/z"));
            Files.writeString(d.resolve("x/y/f"), "1");
            List<String> w;
            try (Stream<Path> s = Files.walk(d)) {
                w = s.map(SbrW2Fs::rel).sorted().collect(Collectors.toList());
            }
            long n;
            try (Stream<Path> s = Files.list(d.resolve("x/y"))) {
                n = s.count();
            }
            rm(d);
            return w + " list=" + n;
        });
        row("channel", () -> {
            Path d = fresh("ch");
            Path f = d.resolve("c");
            try (SeekableByteChannel c = Files.newByteChannel(f, EnumSet.of(StandardOpenOption.CREATE_NEW,
                    StandardOpenOption.WRITE, StandardOpenOption.READ))) {
                c.write(ByteBuffer.wrap("abcdef".getBytes(StandardCharsets.UTF_8)));
                c.position(2);
                ByteBuffer b = ByteBuffer.allocate(3);
                c.read(b);
                String r = new String(b.array(), StandardCharsets.UTF_8) + " size=" + c.size()
                        + " again=" + safe(() -> { Files.newByteChannel(f, StandardOpenOption.CREATE_NEW,
                                StandardOpenOption.WRITE).close(); return "no"; });
                c.close();
                rm(d);
                return r;
            }
        });
        row("copyMove", () -> {
            Path d = fresh("cm");
            Path src = Files.writeString(d.resolve("src"), "payload");
            Path empty = Files.createFile(d.resolve("empty"));
            Files.createDirectory(d.resolve("sub0"));
            Files.setLastModifiedTime(src, FileTime.fromMillis(1_600_000_000_000L));
            Path c1 = Files.copy(src, d.resolve("c1"));
            Path c2 = Files.copy(src, d.resolve("c2"), StandardCopyOption.COPY_ATTRIBUTES);
            Path c3 = Files.copy(empty, d.resolve("c3"));
            String exists = safe(() -> "" + Files.copy(src, d.resolve("c1")));
            Files.copy(empty, d.resolve("c1"), StandardCopyOption.REPLACE_EXISTING);
            Path m = Files.move(d.resolve("c2"), d.resolve("m"));
            Path dc = Files.copy(d.resolve("sub0"), d.resolve("sub0c"), StandardCopyOption.REPLACE_EXISTING);
            String r = Files.readString(c1).length() + " " + Files.readString(m)
                    + " c2attr=" + Files.getLastModifiedTime(m).toMillis()
                    + " c1time=" + (Files.getLastModifiedTime(d.resolve("c1")).toMillis() == 1_600_000_000_000L)
                    + " c3=" + Files.size(c3) + " exists=" + exists + " c2gone=" + Files.exists(d.resolve("c2"))
                    + " dircopy=" + Files.isDirectory(dc);
            rm(d);
            return r;
        });
        row("delete", () -> {
            Path d = fresh("del");
            Files.createDirectories(d.resolve("full/x"));
            Files.createFile(d.resolve("f"));
            String r = safe(() -> { Files.delete(d.resolve("full")); return "no"; })
                    + " " + safe(() -> { Files.delete(d.resolve("nope")); return "no"; })
                    + " " + Files.deleteIfExists(d.resolve("f")) + " " + Files.deleteIfExists(d.resolve("f"))
                    + " mkExisting=" + safe(() -> { Files.createDirectory(d.resolve("full")); return "no"; });
            rm(d);
            return r;
        });
        row("zipfs", () -> {
            Path d = fresh("zip");
            Path z = d.resolve("a.zip");
            try (FileSystem zfs = FileSystems.newFileSystem(z, Map.of("create", "true"))) {
                Files.writeString(zfs.getPath("/in.txt"), "zipped");
            }
            String r;
            try (FileSystem zfs = FileSystems.newFileSystem(z)) {
                r = zfs.provider().getScheme() + " " + Files.readString(zfs.getPath("/in.txt"));
            }
            rm(d);
            return r;
        });

        // ---- SBR wave 5, lane fs2: the rows wave 2 held -------------------
        row("watch.lifecycle", () -> {
            Path d = fresh("watch");
            StringBuilder b = new StringBuilder();
            WatchKey k;
            WatchService ws = FileSystems.getDefault().newWatchService();
            try {
                k = d.register(ws, StandardWatchEventKinds.ENTRY_CREATE,
                        StandardWatchEventKinds.ENTRY_DELETE, StandardWatchEventKinds.ENTRY_MODIFY);
                b.append(ws.getClass().getName()).append(" key=").append(k.getClass().getName())
                 .append(" valid=").append(k.isValid()).append(" watchable=").append(k.watchable() == d)
                 .append(" poll0=").append(ws.poll())
                 .append(" poll50=").append(ws.poll(50, TimeUnit.MILLISECONDS))
                 .append(" again=").append(d.register(ws, StandardWatchEventKinds.ENTRY_CREATE,
                         StandardWatchEventKinds.ENTRY_DELETE, StandardWatchEventKinds.ENTRY_MODIFY) == k)
                 .append(" fresh=").append(k.pollEvents());
                Files.createFile(d.resolve("a"));
                WatchKey got = ws.poll(10, TimeUnit.SECONDS);
                b.append(" got=").append(got == k).append(" ev=").append(events(got))
                 .append(" reset=").append(k.reset());
                Files.delete(d.resolve("a"));
                got = ws.poll(10, TimeUnit.SECONDS);
                b.append(" got2=").append(got == k).append(" ev=").append(events(got))
                 .append(" reset=").append(k.reset());
                // `take()` bounded by a guard that closes the service after
                // 10 s (a broken take then answers ClosedWatchServiceException
                // instead of hanging the probe).
                Files.createFile(d.resolve("file0"));
                Thread guard = new Thread(() -> {
                    try {
                        Thread.sleep(10_000);
                        ws.close();
                    } catch (Exception ignored) {
                    }
                });
                guard.setDaemon(true);
                guard.start();
                got = ws.take();
                guard.interrupt();
                b.append(" take=").append(got == k).append(" ev=").append(events(got))
                 .append(" reset=").append(k.reset());
                k.cancel();
                b.append(" cancelled=").append(k.isValid()).append(" resetCancelled=").append(k.reset())
                 .append(" pollAfterCancel=").append(ws.poll());
                Path f = Files.createFile(d.resolve("file"));
                b.append(" regFile=").append(safe(() -> "" + f.register(ws, StandardWatchEventKinds.ENTRY_CREATE)))
                 .append(" noKinds=").append(safe(() -> "" + d.register(ws)))
                 .append(" missing=").append(safe(() -> "" + d.resolve("nope").register(ws,
                         StandardWatchEventKinds.ENTRY_CREATE)));
                k = d.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
                b.append(" reReg=").append(k.isValid());
            } finally {
                ws.close();
            }
            WatchKey closedKey = k;
            b.append(" afterClose.valid=").append(closedKey.isValid())
             .append(" poll=").append(safe(() -> "" + ws.poll()))
             .append(" take=").append(safe(() -> "" + ws.take()))
             .append(" reg=").append(safe(() -> "" + d.register(ws, StandardWatchEventKinds.ENTRY_CREATE)))
             .append(" close2=").append(safe(() -> { ws.close(); return "ok"; }));
            rm(d);
            return b.toString();
        });
        row("fileStore", () -> {
            Path d = fresh("fstore");
            FileStore s = Files.getFileStore(d);
            FileStore s2 = Files.getFileStore(BASE);
            String r = s.getClass().getName() + " eq=" + s.equals(s2) + " hashEq=" + (s.hashCode() == s2.hashCode())
                    + " name=" + s.name() + " type=" + s.type() + " ro=" + s.isReadOnly()
                    + " total>0=" + (s.getTotalSpace() > 0) + " usable<=total=" + (s.getUsableSpace() <= s.getTotalSpace())
                    + " attrTotal=" + s.getAttribute("totalSpace").equals(s.getTotalSpace())
                    + " block=" + s.getBlockSize()
                    + " views=" + s.supportsFileAttributeView("basic") + s.supportsFileAttributeView("posix")
                    + s.supportsFileAttributeView("unix") + s.supportsFileAttributeView("owner")
                    + s.supportsFileAttributeView("dos") + s.supportsFileAttributeView("acl")
                    + s.supportsFileAttributeView("user")
                    + " cviews=" + s.supportsFileAttributeView(BasicFileAttributeView.class)
                    + s.supportsFileAttributeView(PosixFileAttributeView.class)
                    + s.supportsFileAttributeView(FileOwnerAttributeView.class)
                    + s.supportsFileAttributeView(DosFileAttributeView.class)
                    + s.supportsFileAttributeView(AclFileAttributeView.class)
                    + s.supportsFileAttributeView(UserDefinedFileAttributeView.class)
                    + " fsav=" + s.getFileStoreAttributeView(FileStoreAttributeView.class)
                    + " fsavNull=" + safe(() -> "" + s.getFileStoreAttributeView(null))
                    + " str=" + s
                    + " badAttr=" + safe(() -> "" + s.getAttribute("foo:bar"))
                    + " missing=" + safe(() -> "" + Files.getFileStore(d.resolve("nope")))
                    + " viaProvider=" + d.getFileSystem().provider().getFileStore(d).equals(s);
            rm(d);
            return r;
        });
        row("defaultProvider", () -> {
            Class<?> c = Class.forName("sun.nio.fs.DefaultFileSystemProvider");
            Method inst = c.getMethod("instance");
            Method tfs = c.getMethod("theFileSystem");
            inst.setAccessible(true);
            tfs.setAccessible(true);
            Object p = inst.invoke(null);
            Object f = tfs.invoke(null);
            return p.getClass().getName() + " instIsDefault=" + (p == FileSystems.getDefault().provider())
                    + " tfsIsDefault=" + (f == FileSystems.getDefault())
                    + " installed0=" + (FileSystemProvider.installedProviders().get(0) == p)
                    + " stable=" + (inst.invoke(null) == p && tfs.invoke(null) == f)
                    + " pathFs=" + (Path.of("x").getFileSystem() == f)
                    + " uriFs=" + (Path.of(URI.create("file:///x")).getFileSystem() == f);
        });

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + units(first[i]));
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + units(v));
                    }
                    drift++;
                }
            }
        }
        rm(BASE);
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }
}
