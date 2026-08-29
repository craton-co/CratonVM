import java.io.*;
import java.nio.ByteBuffer;
import java.nio.charset.*;
import java.nio.file.*;
import java.nio.file.attribute.*;
import java.util.*;
import java.util.stream.*;

/** L4 -- `java.nio.file.Files` and `Path`, asked at their CONTRACT EDGES.
 *
 *  `Files` is almost entirely refusals: nearly every method has a documented
 *  exception for a missing file, for a directory where a file was expected, for
 *  a file where a directory was expected, and for a target that already exists.
 *  Those types are the API -- `NoSuchFileException` and `FileAlreadyExists-
 *  Exception` are both `FileSystemException`s carrying the offending path, and
 *  a shim answering `IOException` to everything satisfies every `throws` clause
 *  while telling the caller nothing it can act on.
 *
 *  The `java.io` twin of each is deliberately asked alongside, because they
 *  DISAGREE by design and a shim that routes one through the other collapses
 *  the difference:
 *
 *      Files.readAllBytes(missing)      NoSuchFileException
 *      new FileInputStream(missing)     FileNotFoundException
 *
 *  Ordering hygiene: every directory listing is SORTED before printing --
 *  directory order belongs to the filesystem, not to either VM.
 */
public class L4FilesSweep {
    static int rows = 0;
    static final String CWD = cwd();
    /** Deliberately NOT getParentFile(): that method is under test here,
     *  and using it to build the diff token made one defect in it rename
     *  the token and report six unrelated rows as differences. */
    static String cwd() {
        String s = new File(".").getAbsolutePath();
        if (s.endsWith(File.separator + ".")) s = s.substring(0, s.length() - 2);
        return s;
    }

    static String esc(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static String rel(String s) {
        if (s == null) return "null";
        return esc(s.replace(CWD, "<CWD>"));
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(esc(tag) + " |" + rel(String.valueOf(v)) + "|");
    }
    /** Print the exception TYPE. Messages are not part of the contract and the
     *  two VMs are entitled to word them differently. */
    static void t(String tag, ThrowingRun r) {
        rows++;
        try { r.run(); System.out.println(esc(tag) + " |no-throw|"); }
        catch (Throwable e) { System.out.println(esc(tag) + " |THREW " + e.getClass().getName() + "|"); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    static String sorted(Collection<String> c) {
        List<String> l = new ArrayList<>(c);
        Collections.sort(l);
        return l.toString();
    }
    static String hex(byte[] a) {
        if (a == null) return "null";
        StringBuilder s = new StringBuilder();
        for (byte x : a) s.append(String.format("%02x", x));
        return s.toString();
    }

    static Path BASE;
    static Path file, empty, dir, inner, leaf, missing, missingDir;

    static void rmrf(Path p) {
        try {
            if (Files.isDirectory(p)) {
                try (DirectoryStream<Path> s = Files.newDirectoryStream(p)) {
                    for (Path k : s) rmrf(k);
                }
            }
            Files.deleteIfExists(p);
        } catch (IOException e) { }
    }

    static void fixture() throws IOException {
        BASE = Paths.get("l4nio");
        rmrf(BASE);
        Files.createDirectories(BASE);
        file = BASE.resolve("file.txt");
        empty = BASE.resolve("empty.txt");
        dir = BASE.resolve("dir");
        inner = dir.resolve("inner");
        leaf = inner.resolve("leaf.txt");
        missing = BASE.resolve("missing.txt");
        missingDir = BASE.resolve("missing-dir");
        Files.write(file, "hello\nworld\n".getBytes(StandardCharsets.UTF_8));
        Files.write(empty, new byte[0]);
        Files.createDirectories(inner);
        Files.write(leaf, new byte[]{1, 2, 3});
    }

    // ------------------------------------------------------------- path text

    static void pathText() {
        String[] specs = {
            "", ".", "..", "/", "//", "a", "a/b", "a/b/c", "/a", "/a/b", "a/",
            "a//b", "a/./b", "a/../b", "/..", "trailing/", ".hidden", "a.b.c",
        };
        for (String s : specs) {
            Path q = Paths.get(s);
            p("path[" + s + "] toString", q.toString());
            p("path[" + s + "] fileName", q.getFileName());
            p("path[" + s + "] parent", q.getParent());
            p("path[" + s + "] root", q.getRoot());
            p("path[" + s + "] nameCount", q.getNameCount());
            p("path[" + s + "] isAbsolute", q.isAbsolute());
            p("path[" + s + "] normalize", q.normalize());
            p("path[" + s + "] equals-self", q.equals(Paths.get(s)));
            p("path[" + s + "] hash-eq", q.hashCode() == Paths.get(s).hashCode());
        }
        // getName / subpath bounds.
        Path abc = Paths.get("a/b/c");
        p("getName(0)", abc.getName(0));
        p("getName(2)", abc.getName(2));
        t("getName(-1)", () -> abc.getName(-1));
        t("getName(3)", () -> abc.getName(3));
        p("subpath(0,3)", abc.subpath(0, 3));
        p("subpath(1,2)", abc.subpath(1, 2));
        t("subpath(0,0)", () -> abc.subpath(0, 0));
        t("subpath(2,1)", () -> abc.subpath(2, 1));
        t("subpath(0,4)", () -> abc.subpath(0, 4));
        t("subpath(-1,2)", () -> abc.subpath(-1, 2));
        // The empty path is ONE name, not zero -- the rule that surprises.
        Path e = Paths.get("");
        p("empty nameCount", e.getNameCount());
        p("empty getName(0)", e.getName(0));
        p("empty getFileName", e.getFileName());
        // A ROOT has zero names and its own fileName is null.
        Path root = Paths.get("/");
        p("root nameCount", root.getNameCount());
        p("root getFileName", root.getFileName());
        p("root getParent", root.getParent());
        p("root normalize", root.normalize());
        // resolve / resolveSibling / relativize.
        p("resolve relative", abc.resolve("d"));
        p("resolve absolute wins", abc.resolve("/z"));
        p("resolve empty", abc.resolve(""));
        p("resolveSibling", abc.resolveSibling("z"));
        p("resolveSibling empty parent", Paths.get("a").resolveSibling("z"));
        p("relativize forward", Paths.get("a/b").relativize(Paths.get("a/b/c/d")));
        p("relativize backward", Paths.get("a/b/c").relativize(Paths.get("a/b")));
        p("relativize sideways", Paths.get("a/b").relativize(Paths.get("a/c")));
        p("relativize equal", "[" + Paths.get("a/b").relativize(Paths.get("a/b")) + "]");
        p("relativize from empty", Paths.get("").relativize(Paths.get("a")));
        // A relative path and an absolute one cannot be relativized.
        t("relativize rel vs abs", () -> Paths.get("a").relativize(Paths.get("/a")));
        t("relativize abs vs rel", () -> Paths.get("/a").relativize(Paths.get("a")));
        t("relativize null", () -> abc.relativize(null));
        t("resolve null", () -> abc.resolve((Path) null));
        t("Paths.get(null)", () -> Paths.get((String) null));
        t("Path.of(null)", () -> Path.of((String) null));
        // startsWith / endsWith are NAME-wise, not string-wise: "a/bc" does not
        // start with "a/b" even though the string does.
        p("startsWith name", Paths.get("a/bc").startsWith("a/b"));
        p("startsWith prefix", Paths.get("a/bc").startsWith("a"));
        p("startsWith self", abc.startsWith(abc));
        p("startsWith empty", abc.startsWith(""));
        p("endsWith name", Paths.get("ab/c").endsWith("b/c"));
        p("endsWith tail", abc.endsWith("b/c"));
        p("endsWith self", abc.endsWith(abc));
        p("endsWith abs vs rel", Paths.get("/a/b").endsWith("a/b"));
        t("startsWith null", () -> abc.startsWith((Path) null));
        // Ordering and iteration.
        p("compareTo self", abc.compareTo(Paths.get("a/b/c")));
        p("compareTo sign", Integer.signum(Paths.get("a").compareTo(Paths.get("b"))));
        p("equals other type", abc.equals("a/b/c"));
        StringBuilder it = new StringBuilder();
        for (Path q : abc) it.append(q).append(',');
        p("iterator names", it.toString());
        p("Paths.get multi", Paths.get("a", "b", "c"));
        p("Paths.get with empty part", Paths.get("a", "", "c"));
        p("toFile round trip", abc.toFile().toPath().equals(abc));
        p("getFileSystem is default", abc.getFileSystem() == FileSystems.getDefault());
    }

    // ------------------------------------------------------------- existence

    static void existence() throws Exception {
        p("exists file", Files.exists(file));
        p("exists missing", Files.exists(missing));
        p("notExists missing", Files.notExists(missing));
        p("notExists file", Files.notExists(file));
        p("isRegularFile file", Files.isRegularFile(file));
        p("isRegularFile dir", Files.isRegularFile(dir));
        p("isRegularFile missing", Files.isRegularFile(missing));
        p("isDirectory dir", Files.isDirectory(dir));
        p("isDirectory file", Files.isDirectory(file));
        p("isDirectory missing", Files.isDirectory(missing));
        p("isReadable file", Files.isReadable(file));
        p("isReadable missing", Files.isReadable(missing));
        p("isWritable file", Files.isWritable(file));
        p("isExecutable file", Files.isExecutable(file));
        p("isExecutable dir", Files.isExecutable(dir));
        p("isHidden file", Files.isHidden(file));
        p("isSymbolicLink file", Files.isSymbolicLink(file));
        p("size file", Files.size(file));
        p("size empty", Files.size(empty));
        // size of a MISSING file throws; size of a directory does not.
        t("size missing", () -> Files.size(missing));
        p("size dir >= 0", Files.size(dir) >= 0);
        p("isSameFile self", Files.isSameFile(file, file));
        p("isSameFile via dots", Files.isSameFile(file, BASE.resolve("dir/../file.txt")));
        p("isSameFile different", Files.isSameFile(file, empty));
        t("isSameFile missing", () -> Files.isSameFile(missing, file));
        p("mismatch identical", Files.mismatch(file, file));
        p("mismatch different", Files.mismatch(file, empty));
        // Every predicate takes null as an NPE, not as false.
        t("exists(null)", () -> Files.exists(null));
        t("isDirectory(null)", () -> Files.isDirectory(null));
        t("size(null)", () -> Files.size(null));
        t("readAllBytes(null)", () -> Files.readAllBytes(null));
        t("delete(null)", () -> Files.delete(null));
        t("copy(null,null)", () -> Files.copy((Path) null, (Path) null));
        t("createDirectory(null)", () -> Files.createDirectory(null));
        t("newInputStream(null)", () -> Files.newInputStream(null));
    }

    // ------------------------------------------------------------- read/write

    static void readWrite() throws Exception {
        p("readAllBytes", hex(Files.readAllBytes(leaf)));
        p("readAllBytes empty", hex(Files.readAllBytes(empty)));
        p("readString", Files.readString(file));
        p("readAllLines", Files.readAllLines(file));
        p("readAllLines empty", Files.readAllLines(empty));
        p("lines count", Files.lines(file).count());
        // A MISSING file is NoSuchFileException here, FileNotFoundException on
        // the java.io side. The pair is the point.
        t("readAllBytes missing", () -> Files.readAllBytes(missing));
        t("readString missing", () -> Files.readString(missing));
        t("readAllLines missing", () -> Files.readAllLines(missing));
        t("lines missing", () -> Files.lines(missing).count());
        t("FileInputStream missing", () -> new FileInputStream(missing.toFile()).close());
        t("newInputStream missing", () -> Files.newInputStream(missing).close());
        // A DIRECTORY where a file is expected.
        t("readAllBytes dir", () -> Files.readAllBytes(dir));
        t("newInputStream dir", () -> Files.newInputStream(dir).close());
        t("newBufferedReader dir", () -> Files.newBufferedReader(dir).close());
        t("FileInputStream dir", () -> new FileInputStream(dir.toFile()).close());
        t("newOutputStream dir", () -> Files.newOutputStream(dir).close());
        // Malformed input is a decoding failure, not a silent replacement.
        Path bad = BASE.resolve("bad.bin");
        Files.write(bad, new byte[]{(byte) 0xC3, (byte) 0x28});
        t("readString malformed utf8", () -> Files.readString(bad));
        t("readAllLines malformed utf8", () -> Files.readAllLines(bad));
        p("readAllBytes malformed", hex(Files.readAllBytes(bad)));
        // Writes. CREATE_NEW on an existing file is FileAlreadyExistsException;
        // an append leaves the prefix intact; a plain write TRUNCATES.
        Path w = BASE.resolve("w.txt");
        Files.write(w, "abc".getBytes());
        p("write then read", Files.readString(w));
        Files.write(w, "z".getBytes());
        p("write truncates", Files.readString(w));
        Files.write(w, "y".getBytes(), StandardOpenOption.APPEND);
        p("append", Files.readString(w));
        Files.writeString(w, "s");
        p("writeString truncates", Files.readString(w));
        t("write CREATE_NEW existing", () -> Files.write(w, new byte[0], StandardOpenOption.CREATE_NEW));
        t("newOutputStream CREATE_NEW existing", () -> Files.newOutputStream(w, StandardOpenOption.CREATE_NEW).close());
        t("write READ", () -> Files.write(w, new byte[0], StandardOpenOption.READ));
        t("write into missing dir", () -> Files.write(missingDir.resolve("x"), new byte[0]));
        t("write(null bytes)", () -> Files.write(w, (byte[]) null));
        t("newBufferedWriter missing dir", () -> Files.newBufferedWriter(missingDir.resolve("x")).close());
        // A stream opened for read refuses a write and vice versa.
        t("newInputStream WRITE", () -> Files.newInputStream(file, StandardOpenOption.WRITE).close());
        // Copy from and to a stream.
        p("copy(in,path)", Files.copy(new ByteArrayInputStream(new byte[]{4, 5}), BASE.resolve("fromstream.bin")));
        t("copy(in,existing)", () -> Files.copy(new ByteArrayInputStream(new byte[]{6}), BASE.resolve("fromstream.bin")));
        p("copy(in,existing,REPLACE)", Files.copy(new ByteArrayInputStream(new byte[]{6}), BASE.resolve("fromstream.bin"), StandardCopyOption.REPLACE_EXISTING));
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        p("copy(path,out)", Files.copy(leaf, out));
        p("copy(path,out) content", hex(out.toByteArray()));
        t("copy(missing,out)", () -> Files.copy(missing, new ByteArrayOutputStream()));
    }

    // ------------------------------------------------------------ directories

    static void directories() throws Exception {
        // createDirectory on an EXISTING directory throws; createDirectories
        // does not. That asymmetry is the contract.
        Path d1 = BASE.resolve("d1");
        p("createDirectory fresh", Files.createDirectory(d1).getFileName());
        t("createDirectory existing", () -> Files.createDirectory(d1));
        t("createDirectories existing", () -> Files.createDirectories(d1));
        t("createDirectory nested missing parent", () -> Files.createDirectory(BASE.resolve("nope/deep")));
        p("createDirectories nested", Files.createDirectories(BASE.resolve("n1/n2/n3")).getFileName());
        t("createDirectories over a file", () -> Files.createDirectories(file));
        t("createDirectory over a file", () -> Files.createDirectory(file));
        // createFile.
        Path cf = BASE.resolve("cf.txt");
        p("createFile fresh", Files.createFile(cf).getFileName());
        t("createFile existing", () -> Files.createFile(cf));
        t("createFile missing parent", () -> Files.createFile(BASE.resolve("nope/x")));
        // delete: missing throws NoSuchFile, non-empty dir throws
        // DirectoryNotEmpty, and deleteIfExists answers false rather than
        // throwing for the missing case ONLY.
        t("delete missing", () -> Files.delete(missing));
        p("deleteIfExists missing", Files.deleteIfExists(missing));
        t("delete non-empty dir", () -> Files.delete(dir));
        t("deleteIfExists non-empty dir", () -> Files.deleteIfExists(dir));
        p("delete empty dir", boolThrow(() -> Files.delete(BASE.resolve("n1/n2/n3"))));
        p("delete file", boolThrow(() -> Files.delete(cf)));
        // Listing. A FILE is not a directory: NotDirectoryException, and it is
        // thrown by newDirectoryStream eagerly but by list/walk lazily.
        try (DirectoryStream<Path> s = Files.newDirectoryStream(BASE)) {
            List<String> names = new ArrayList<>();
            for (Path q : s) names.add(q.getFileName().toString());
            p("newDirectoryStream names", sorted(names));
        }
        try (DirectoryStream<Path> s = Files.newDirectoryStream(BASE, "*.txt")) {
            List<String> names = new ArrayList<>();
            for (Path q : s) names.add(q.getFileName().toString());
            p("newDirectoryStream glob", sorted(names));
        }
        t("newDirectoryStream on file", () -> Files.newDirectoryStream(file).close());
        t("newDirectoryStream missing", () -> Files.newDirectoryStream(missingDir).close());
        try (Stream<Path> s = Files.list(BASE)) {
            p("list names", sorted(s.map(q -> q.getFileName().toString()).collect(Collectors.toList())));
        }
        t("list on file", () -> Files.list(file).count());
        t("list missing", () -> Files.list(missingDir).count());
        // walk: depth 0 is the start node only; a negative depth is refused.
        try (Stream<Path> s = Files.walk(dir)) {
            p("walk dir", sorted(s.map(q -> BASE.relativize(q).toString()).collect(Collectors.toList())));
        }
        try (Stream<Path> s = Files.walk(dir, 0)) {
            p("walk depth 0", sorted(s.map(q -> BASE.relativize(q).toString()).collect(Collectors.toList())));
        }
        try (Stream<Path> s = Files.walk(dir, 1)) {
            p("walk depth 1", sorted(s.map(q -> BASE.relativize(q).toString()).collect(Collectors.toList())));
        }
        t("walk depth -1", () -> Files.walk(dir, -1).count());
        try (Stream<Path> s = Files.walk(file)) {
            p("walk a file", sorted(s.map(q -> BASE.relativize(q).toString()).collect(Collectors.toList())));
        }
        t("walk missing", () -> Files.walk(missingDir).count());
        try (Stream<Path> s = Files.find(dir, 3, (q, at) -> at.isRegularFile())) {
            p("find regular files", sorted(s.map(q -> BASE.relativize(q).toString()).collect(Collectors.toList())));
        }
        t("find depth -1", () -> Files.find(dir, -1, (q, at) -> true).count());
        // walkFileTree visits in a defined ORDER of kinds even if not of names.
        List<String> events = new ArrayList<>();
        Files.walkFileTree(dir, new SimpleFileVisitor<Path>() {
            public FileVisitResult preVisitDirectory(Path p2, BasicFileAttributes a) {
                events.add("pre:" + BASE.relativize(p2)); return FileVisitResult.CONTINUE;
            }
            public FileVisitResult visitFile(Path p2, BasicFileAttributes a) {
                events.add("file:" + BASE.relativize(p2)); return FileVisitResult.CONTINUE;
            }
            public FileVisitResult postVisitDirectory(Path p2, IOException e) {
                events.add("post:" + BASE.relativize(p2)); return FileVisitResult.CONTINUE;
            }
        });
        p("walkFileTree events", sorted(events));
        t("walkFileTree missing", () -> Files.walkFileTree(missingDir, new SimpleFileVisitor<Path>() {}));
    }

    static boolean boolThrow(ThrowingRun r) {
        try { r.run(); return true; } catch (Throwable e) { return false; }
    }

    // ------------------------------------------------------------- copy/move

    static void copyMove() throws Exception {
        Path a = BASE.resolve("cm-a.txt");
        Path b = BASE.resolve("cm-b.txt");
        Files.write(a, "A".getBytes());
        Files.write(b, "B".getBytes());
        t("copy onto existing", () -> Files.copy(a, b));
        p("target untouched after refusal", Files.readString(b));
        p("copy REPLACE_EXISTING", Files.copy(a, b, StandardCopyOption.REPLACE_EXISTING).getFileName());
        p("target replaced", Files.readString(b));
        p("source still there", Files.exists(a));
        t("copy missing source", () -> Files.copy(missing, BASE.resolve("cm-c.txt")));
        t("copy into missing dir", () -> Files.copy(a, missingDir.resolve("x")));
        p("copy self", boolThrow(() -> Files.copy(a, a)));
        // Copying a DIRECTORY copies the directory, not its contents.
        Path dc = BASE.resolve("dircopy");
        p("copy a directory", Files.copy(dir, dc).getFileName());
        p("copied directory is empty", boolThrow(() -> Files.newDirectoryStream(dc).iterator().hasNext()));
        try (DirectoryStream<Path> s = Files.newDirectoryStream(dc)) {
            p("copied directory has no children", s.iterator().hasNext());
        }
        t("copy dir onto non-empty dir", () -> Files.copy(dir, dir.getParent(), StandardCopyOption.REPLACE_EXISTING));
        // move.
        Path m1 = BASE.resolve("mv-1.txt");
        Path m2 = BASE.resolve("mv-2.txt");
        Files.write(m1, "M".getBytes());
        p("move fresh", Files.move(m1, m2).getFileName());
        p("source gone", Files.exists(m1));
        p("target present", Files.readString(m2));
        Files.write(m1, "N".getBytes());
        t("move onto existing", () -> Files.move(m1, m2));
        p("move REPLACE_EXISTING", Files.move(m1, m2, StandardCopyOption.REPLACE_EXISTING).getFileName());
        p("after replace", Files.readString(m2));
        t("move missing", () -> Files.move(missing, BASE.resolve("mv-3.txt")));
        t("move into missing dir", () -> Files.move(m2, missingDir.resolve("x")));
        p("move self", boolThrow(() -> Files.move(m2, m2)));
        t("copy(null option)", () -> Files.copy(a, BASE.resolve("cm-d.txt"), (CopyOption[]) null));
    }

    // ------------------------------------------------------------ attributes

    static void attributes() throws Exception {
        BasicFileAttributes at = Files.readAttributes(file, BasicFileAttributes.class);
        p("attrs isRegularFile", at.isRegularFile());
        p("attrs isDirectory", at.isDirectory());
        p("attrs isSymbolicLink", at.isSymbolicLink());
        p("attrs isOther", at.isOther());
        p("attrs size", at.size());
        p("attrs lastModified > 0", at.lastModifiedTime().toMillis() > 0);
        p("dir attrs isDirectory", Files.readAttributes(dir, BasicFileAttributes.class).isDirectory());
        t("readAttributes missing", () -> Files.readAttributes(missing, BasicFileAttributes.class));
        t("readAttributes null type", () -> Files.readAttributes(file, (Class<BasicFileAttributes>) null));
        p("getAttribute basic:size", Files.getAttribute(file, "basic:size"));
        p("getAttribute size", Files.getAttribute(file, "size"));
        t("getAttribute unknown", () -> Files.getAttribute(file, "basic:nosuch"));
        t("getAttribute unknown view", () -> Files.getAttribute(file, "nosuchview:size"));
        t("getAttribute missing file", () -> Files.getAttribute(missing, "size"));
        Map<String, Object> m = Files.readAttributes(file, "basic:size,isDirectory");
        p("readAttributes map keys", sorted(m.keySet()));
        p("readAttributes map size", m.get("size"));
        t("readAttributes bad spec", () -> Files.readAttributes(file, "basic:nosuch"));
        // Times round-trip through setLastModifiedTime.
        FileTime ft = FileTime.fromMillis(1_000_000_000L);
        p("setLastModifiedTime", Files.setLastModifiedTime(file, ft).getFileName());
        p("getLastModifiedTime reads back", Files.getLastModifiedTime(file).toMillis());
        t("setLastModifiedTime missing", () -> Files.setLastModifiedTime(missing, ft));
        t("getLastModifiedTime missing", () -> Files.getLastModifiedTime(missing));
        p("getFileAttributeView basic non-null", Files.getFileAttributeView(file, BasicFileAttributeView.class) != null);
        p("getFileAttributeView name", Files.getFileAttributeView(file, BasicFileAttributeView.class).name());
        t("probeContentType txt", () -> {
            String ct = Files.probeContentType(file);
            if (!(ct == null || ct.startsWith("text"))) throw new IllegalStateException(ct);
        });
        p("getFileStore non-null", Files.getFileStore(file) != null);
        p("fileStore totalSpace > 0", Files.getFileStore(file).getTotalSpace() > 0);
        t("getFileStore missing", () -> Files.getFileStore(missing));
        // The default filesystem's own shape.
        FileSystem fs = FileSystems.getDefault();
        p("fs separator", fs.getSeparator());
        p("fs isOpen", fs.isOpen());
        p("fs isReadOnly", fs.isReadOnly());
        p("fs supports basic", fs.supportedFileAttributeViews().contains("basic"));
        p("fs rootDirectories non-empty", fs.getRootDirectories().iterator().hasNext());
        p("fs getPath", fs.getPath("a", "b"));
        p("fs provider scheme", fs.provider().getScheme());
        t("fs close default", () -> fs.close());
        p("fs still open", fs.isOpen());
        p("pathMatcher glob", fs.getPathMatcher("glob:*.txt").matches(Paths.get("a.txt")));
        p("pathMatcher glob no", fs.getPathMatcher("glob:*.txt").matches(Paths.get("a.bin")));
        p("pathMatcher regex", fs.getPathMatcher("regex:.*\\.txt").matches(Paths.get("a.txt")));
        t("pathMatcher unknown syntax", () -> fs.getPathMatcher("nosuch:x"));
        t("pathMatcher no colon", () -> fs.getPathMatcher("nocolon"));
    }

    // ------------------------------------------------------------- channels

    static void channels() throws Exception {
        Path c = BASE.resolve("chan.bin");
        try (java.nio.channels.SeekableByteChannel ch = Files.newByteChannel(c,
                StandardOpenOption.CREATE, StandardOpenOption.WRITE, StandardOpenOption.READ)) {
            p("channel isOpen", ch.isOpen());
            p("channel initial size", ch.size());
            p("channel write", ch.write(ByteBuffer.wrap(new byte[]{1, 2, 3, 4})));
            p("channel position after write", ch.position());
            p("channel size after write", ch.size());
            ch.position(1);
            ByteBuffer rb = ByteBuffer.allocate(2);
            p("channel read", ch.read(rb));
            p("channel read content", hex(rb.array()));
            p("channel truncate", ch.truncate(2).size());
            p("channel position after truncate", ch.position());
            ByteBuffer past = ByteBuffer.allocate(4);
            ch.position(10);
            p("channel read past end", ch.read(past));
        }
        t("newByteChannel missing without CREATE", () -> Files.newByteChannel(BASE.resolve("nope.bin")).close());
        t("newByteChannel on dir for write", () -> Files.newByteChannel(dir, StandardOpenOption.WRITE).close());
        t("channel negative position", () -> {
            try (java.nio.channels.SeekableByteChannel ch = Files.newByteChannel(c)) { ch.position(-1); }
        });
        t("channel negative truncate", () -> {
            try (java.nio.channels.SeekableByteChannel ch = Files.newByteChannel(c, StandardOpenOption.WRITE)) { ch.truncate(-1); }
        });
        t("closed channel read", () -> {
            java.nio.channels.SeekableByteChannel ch = Files.newByteChannel(c);
            ch.close();
            ch.read(ByteBuffer.allocate(1));
        });
    }

    public static void main(String[] a) throws Exception {
        try {
            fixture();
            pathText();
            existence();
            readWrite();
            directories();
            copyMove();
            attributes();
            channels();
        } finally {
            rmrf(BASE);
        }
        System.out.println("rows " + rows);
        System.out.println("DONE L4FilesSweep");
    }
}
