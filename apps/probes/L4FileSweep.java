import java.io.*;
import java.net.URI;
import java.util.*;

/** L4 -- `java.io.File`, asked at its CONTRACT EDGES.
 *
 *  Four families into this campaign, 510 probed rows and 28 defects, EVERY ONE
 *  on a contract edge and not one a wrong answer to an ordinary call. So this
 *  probe asks the perimeter: nulls, a missing file, a file where a directory is
 *  expected, a root, a bare name, an empty path, a trailing separator.
 *
 *  Hygiene, each item learned by getting it wrong somewhere in this campaign:
 *
 *   * every path printed is made RELATIVE to the process's own directory, so
 *     the diff is not a function of where the two VMs were launched;
 *   * `list()` / `listFiles()` results are SORTED -- directory order is a
 *     filesystem choice neither VM owns;
 *   * no timestamps, no free-space numbers, no identity hashes: a value the two
 *     VMs may legitimately choose independently is a harness artefact, and four
 *     of those have already been paid for;
 *   * the fixture tree is torn down and rebuilt at entry, so a run is
 *     self-contained and the second VM never sees the first one's leftovers;
 *   * the last line is a ROW COUNT. A run that dies partway produces a short
 *     file whose missing tail `diff` reports as ordinary `<` lines.
 */
public class L4FileSweep {
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
    /** Replace the process's own directory with a stable token. */
    static String rel(String s) {
        if (s == null) return "null";
        return esc(s.replace(CWD, "<CWD>"));
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(esc(tag) + " |" + rel(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        rows++;
        try { r.run(); System.out.println(esc(tag) + " |no-throw|"); }
        catch (Throwable e) { System.out.println(esc(tag) + " |THREW " + e.getClass().getName() + "|"); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    static String arr(String[] a) {
        if (a == null) return "null";
        String[] c = a.clone();
        Arrays.sort(c);
        return Arrays.toString(c);
    }
    static String farr(File[] a) {
        if (a == null) return "null";
        String[] n = new String[a.length];
        for (int i = 0; i < a.length; i++) n[i] = a[i].getName();
        Arrays.sort(n);
        return Arrays.toString(n);
    }

    static final File BASE = new File("l4work");

    static void rmrf(File f) {
        File[] kids = f.listFiles();
        if (kids != null) for (File k : kids) rmrf(k);
        f.delete();
    }

    static void fixture() throws Exception {
        rmrf(BASE);
        BASE.mkdirs();
        new File(BASE, "dir").mkdir();
        new File(BASE, "dir/inner").mkdir();
        try (FileOutputStream o = new FileOutputStream(new File(BASE, "dir/inner/leaf.txt"))) { o.write(new byte[]{1, 2, 3}); }
        try (FileOutputStream o = new FileOutputStream(new File(BASE, "plain.txt"))) { o.write(new byte[]{7, 7}); }
        try (FileOutputStream o = new FileOutputStream(new File(BASE, "empty.txt"))) { }
        new File(BASE, "emptydir").mkdir();
    }

    // ---------------------------------------------------------------- naming

    /** The path-string surface. A pure function of the string -- no disk. */
    static void naming() {
        // The BACKSLASH rows are here because a Linux-shaped probe forgets
        // them, and the first version of this file did. `\\` is a path
        // separator on Windows and an ORDINARY FILENAME CHARACTER on Unix, so
        // every one of these asks a different question on each host and both
        // are the oracle's. They found 46 differing rows on Linux.
        String[] paths = {
            "", ".", "..", "/", "//", "///", "a", "a/b", "a/b/c", "/a", "/a/b",
            "a/", "a//b", "a/./b", "a/../b", "/a/", "trailing/", "  spaced  ",
            "dot.ext", ".hidden", "a.b.c.d", "/..", "/.", "x/y/../../z",
            "\\", "\\\\", "\\x", "a\\b", "trailing\\", "..\\..\\up",
            "relative\\win\\path", "C:", "C:/", "C:\\", "C:\\x\\y",
            "\\\\server\\share", "\\\\server\\share\\f", "//server/share",
        };
        for (String s : paths) {
            File f = new File(s);
            p("name[" + s + "]", f.getName());
            p("parent[" + s + "]", f.getParent());
            p("path[" + s + "]", f.getPath());
            p("abs?[" + s + "]", f.isAbsolute());
            p("parentFile[" + s + "]", f.getParentFile() == null ? "null" : f.getParentFile().getPath());
            p("toString[" + s + "]", f.toString());
            p("hash-eq-self[" + s + "]", f.hashCode() == new File(s).hashCode());
            p("equals-self[" + s + "]", f.equals(new File(s)));
            // `toURI` belongs in the naming block rather than with the disk
            // rows: it is a pure function of the path string plus the working
            // directory, and it is where `slashify` — which is a no-op on Unix
            // and a rewrite on Windows — becomes visible.
            p("toURI[" + s + "]", f.toURI());
        }
        // getParent AT A ROOT is null, and so is the parent of a bare name.
        p("parent of root", new File("/").getParent());
        p("parent of bare name", new File("bare").getParent());
        p("parent of /bare", new File("/bare").getParent());
        p("parentFile of root", String.valueOf(new File("/").getParentFile()));
        p("name of root", new File("/").getName());
        // Two-argument constructors, including the null-parent rule.
        p("File(null,child)", new File((String) null, "c").getPath());
        p("File(File null,child)", new File((File) null, "c").getPath());
        p("File(\"\",child)", new File("", "c").getPath());
        p("File(\"a\",\"\")", new File("a", "").getPath());
        p("File(\"a/\",\"b\")", new File("a/", "b").getPath());
        p("File(\"/\",\"b\")", new File("/", "b").getPath());
        p("File(File(a),b)", new File(new File("a"), "b").getPath());
        t("File((String)null)", () -> new File((String) null));
        t("File(parent,null)", () -> new File("a", (String) null));
        t("File(File,null)", () -> new File(new File("a"), (String) null));
        // Comparison and ordering.
        p("compareTo self", new File("a").compareTo(new File("a")));
        p("compareTo a<b", Integer.signum(new File("a").compareTo(new File("b"))));
        p("compareTo b>a", Integer.signum(new File("b").compareTo(new File("a"))));
        p("equals other type", new File("a").equals("a"));
        p("equals null", new File("a").equals(null));
        t("compareTo null", () -> new File("a").compareTo(null));
        // Static shape.
        p("separator", File.separator);
        p("separatorChar", String.valueOf(File.separatorChar));
        p("pathSeparator", File.pathSeparator);
        p("listRoots length>0", File.listRoots().length > 0);
    }

    // ------------------------------------------------------------ absolutes

    static void absolutes() throws Exception {
        File missing = new File(BASE, "does-not-exist");
        File plain = new File(BASE, "plain.txt");
        p("abs of relative ends with path", plain.getAbsolutePath().endsWith("l4work/plain.txt"));
        p("abs of relative", plain.getAbsolutePath());
        p("absFile equals abs", plain.getAbsoluteFile().getPath().equals(plain.getAbsolutePath()));
        p("isAbsolute relative", plain.isAbsolute());
        p("isAbsolute of abs", plain.getAbsoluteFile().isAbsolute());
        // A canonical path must be computable for a file that does NOT exist,
        // and must collapse `.` and `..` where an absolute path does not.
        p("canonical missing", missing.getCanonicalPath());
        p("canonical missing == absolute", missing.getCanonicalPath().equals(missing.getAbsolutePath()));
        File dotty = new File(BASE, "dir/../plain.txt");
        p("absolute keeps ..", dotty.getAbsolutePath().contains(".."));
        p("canonical drops ..", dotty.getCanonicalPath().contains(".."));
        p("canonical of dotty", dotty.getCanonicalPath());
        p("canonicalFile equals canonicalPath", dotty.getCanonicalFile().getPath().equals(dotty.getCanonicalPath()));
        p("canonical of .", new File(".").getCanonicalPath().equals(CWD));
        p("canonical of trailing slash", new File("l4work/").getCanonicalPath().endsWith("l4work"));
        p("canonical of empty", new File("").getCanonicalPath().equals(CWD));
        p("absolute of empty", new File("").getAbsolutePath().equals(CWD));
        // toURI / File(URI) round trip. A DIRECTORY's URI ends with a slash.
        p("uri file ends", plain.toURI().toString().endsWith("l4work/plain.txt"));
        p("uri dir ends with slash", new File(BASE, "dir").toURI().toString().endsWith("/"));
        p("uri missing ends with slash", missing.toURI().toString().endsWith("/"));
        p("uri scheme", plain.toURI().getScheme());
        p("uri round trip", new File(plain.toURI()).getPath().equals(plain.getAbsolutePath()));
        t("File(relative URI)", () -> new File(new URI("foo/bar")));
        t("File(non-file URI)", () -> new File(new URI("http://x/y")));
        t("File((URI)null)", () -> new File((URI) null));
        p("toPath round trip", plain.toPath().toFile().getPath().equals(plain.getPath()));
        // `toURI()` renders through `java.net.URI`'s multi-argument
        // constructor, whose `quote` escapes a character BELOW U+0080 only when
        // the mask rejects it and appends everything above it unchanged — only
        // `toASCIIString()` encodes the rest. So a space is `%20` and an
        // e-acute is itself. Both halves are asked, because a shim that
        // percent-encodes everything gets the first row right and the second
        // wrong, which is what this VM did.
        p("uri escapes a space", new File(BASE, "a b.txt").toURI().toString().endsWith("a%20b.txt"));
        p("uri escapes a hash", new File(BASE, "a#b.txt").toURI().toString().endsWith("a%23b.txt"));
        p("uri keeps non-ascii", new File(BASE, "\u00e9\u4e2d.txt").toURI().toString());
        p("uri toASCIIString encodes non-ascii",
          new File(BASE, "\u00e9\u4e2d.txt").toURI().toASCIIString().endsWith("%C3%A9%E4%B8%AD.txt"));
        p("uri round trip non-ascii",
          new File(new File(BASE, "\u00e9\u4e2d.txt").toURI()).getName());
    }

    // ------------------------------------------------------------ existence

    static void existence() throws Exception {
        File missing = new File(BASE, "does-not-exist");
        File plain = new File(BASE, "plain.txt");
        File dir = new File(BASE, "dir");
        File empty = new File(BASE, "empty.txt");

        p("exists missing", missing.exists());
        p("exists plain", plain.exists());
        p("isFile missing", missing.isFile());
        p("isDirectory missing", missing.isDirectory());
        p("isFile dir", dir.isFile());
        p("isDirectory dir", dir.isDirectory());
        p("isFile plain", plain.isFile());
        // A missing file answers ZERO, not an exception and not -1.
        p("length missing", missing.length());
        p("length plain", plain.length());
        p("length empty file", empty.length());
        p("length of dir is not negative", dir.length() >= 0);
        p("lastModified missing", missing.lastModified());
        p("lastModified plain > 0", plain.lastModified() > 0);
        p("canRead missing", missing.canRead());
        p("canWrite missing", missing.canWrite());
        p("canExecute missing", missing.canExecute());
        p("canRead plain", plain.canRead());
        p("isHidden plain", plain.isHidden());
        p("isHidden dotfile", new File(BASE, ".dot").isHidden());
        p("isHidden missing", missing.isHidden());
        // list / listFiles on a FILE is null -- not an empty array.
        p("list on file", arr(plain.list()));
        p("listFiles on file", farr(plain.listFiles()));
        p("list on missing", arr(missing.list()));
        p("listFiles on missing", farr(missing.listFiles()));
        p("list on empty dir", arr(new File(BASE, "emptydir").list()));
        p("listFiles on empty dir length", new File(BASE, "emptydir").listFiles().length);
        p("list on dir", arr(dir.list()));
        p("listFiles on dir", farr(dir.listFiles()));
        p("listFiles(FileFilter) all", farr(dir.listFiles((FileFilter) f -> true)));
        p("listFiles(FileFilter) none", farr(dir.listFiles((FileFilter) f -> false)));
        p("listFiles(FilenameFilter)", farr(dir.listFiles((FilenameFilter) (d, n) -> n.startsWith("in"))));
        p("list(FilenameFilter)", arr(dir.list((d, n) -> true)));
        // A NULL filter means "no filtering", not NPE.
        p("listFiles((FileFilter)null)", farr(dir.listFiles((FileFilter) null)));
        p("listFiles((FilenameFilter)null)", farr(dir.listFiles((FilenameFilter) null)));
        p("list(null)", arr(dir.list(null)));
        p("listFiles(FileFilter) on file", farr(plain.listFiles((FileFilter) null)));
        // Space accessors on a missing path answer 0; on a live one, non-zero.
        p("totalSpace missing is 0", missing.getTotalSpace() == 0);
        p("totalSpace dir > 0", dir.getTotalSpace() > 0);
        p("usableSpace dir >= 0", dir.getUsableSpace() >= 0);
        p("freeSpace missing is 0", missing.getFreeSpace() == 0);
    }

    // -------------------------------------------------------------- mutation

    static void mutation() throws Exception {
        File dir = new File(BASE, "dir");
        File plain = new File(BASE, "plain.txt");

        // createNewFile: true the first time, FALSE (not an exception) after.
        File made = new File(BASE, "made.txt");
        p("createNewFile first", made.createNewFile());
        p("createNewFile again", made.createNewFile());
        p("createNewFile length", made.length());
        t("createNewFile in missing dir", () -> new File(BASE, "nope/deep.txt").createNewFile());
        // mkdir on an existing directory is FALSE, not an exception.
        File md = new File(BASE, "md");
        p("mkdir first", md.mkdir());
        p("mkdir again", md.mkdir());
        p("mkdir nested without parent", new File(BASE, "no/such/deep").mkdir());
        p("mkdirs nested", new File(BASE, "m1/m2/m3").mkdirs());
        p("mkdirs again", new File(BASE, "m1/m2/m3").mkdirs());
        p("mkdirs on existing file", new File(BASE, "plain.txt").mkdirs());
        // delete on a NON-EMPTY directory is FALSE, not an exception.
        p("delete non-empty dir", dir.delete());
        p("non-empty dir still there", dir.isDirectory());
        p("delete empty dir", new File(BASE, "m1/m2/m3").delete());
        p("delete missing", new File(BASE, "never").delete());
        p("delete file", made.delete());
        p("deleted file gone", made.exists());

        // renameTo. Same directory succeeds; onto an existing NAME on POSIX
        // replaces; a missing source is false; a missing target directory is
        // false rather than an exception.
        File src = new File(BASE, "ren-src.txt");
        try (FileOutputStream o = new FileOutputStream(src)) { o.write(5); }
        File dst = new File(BASE, "ren-dst.txt");
        p("renameTo fresh name", src.renameTo(dst));
        p("source gone after rename", src.exists());
        p("target present after rename", dst.exists());
        p("renameTo missing source", new File(BASE, "never2").renameTo(new File(BASE, "x")));
        p("renameTo into missing dir", dst.renameTo(new File(BASE, "nope/deep.txt")));
        p("renameTo self", dst.renameTo(dst));
        t("renameTo null", () -> dst.renameTo(null));
        // setLastModified: a NEGATIVE time is an IllegalArgumentException.
        p("setLastModified valid", plain.setLastModified(1000000L));
        p("lastModified reads back", plain.lastModified());
        t("setLastModified negative", () -> plain.setLastModified(-1));
        p("setLastModified zero", plain.setLastModified(0L));
        p("setLastModified missing", new File(BASE, "never3").setLastModified(1000L));
        // Permission setters, and the readable-again round trip.
        File perm = new File(BASE, "perm.txt");
        try (FileOutputStream o = new FileOutputStream(perm)) { o.write(1); }
        p("setReadOnly", perm.setReadOnly());
        p("canWrite after setReadOnly", perm.canWrite());
        p("setWritable true", perm.setWritable(true));
        p("canWrite after setWritable", perm.canWrite());
        p("setWritable false ownerOnly", perm.setWritable(false, true));
        p("setWritable true ownerOnly", perm.setWritable(true, true));
        p("setReadable false", perm.setReadable(false));
        p("setReadable true", perm.setReadable(true));
        p("setExecutable true", perm.setExecutable(true));
        p("canExecute after set", perm.canExecute());
        p("setExecutable false", perm.setExecutable(false));
        p("setReadOnly missing", new File(BASE, "never4").setReadOnly());
        p("setWritable missing", new File(BASE, "never4").setWritable(true));
        // createTempFile validates its prefix: fewer than three characters is
        // an IllegalArgumentException, and a null prefix is an NPE.
        t("createTempFile short prefix", () -> File.createTempFile("ab", ".t", BASE));
        t("createTempFile null prefix", () -> File.createTempFile(null, ".t", BASE));
        t("createTempFile null suffix ok", () -> File.createTempFile("l4pre", null, BASE).delete());
        t("createTempFile missing dir", () -> File.createTempFile("l4pre", ".t", new File(BASE, "nope")));
        File tf = File.createTempFile("l4pre", ".t", BASE);
        p("createTempFile exists", tf.exists());
        p("createTempFile in given dir", tf.getParentFile().getName());
        p("createTempFile prefix kept", tf.getName().startsWith("l4pre"));
        p("createTempFile suffix kept", tf.getName().endsWith(".t"));
        p("createTempFile length", tf.length());
        tf.delete();
    }

    public static void main(String[] a) throws Exception {
        try {
            fixture();
            naming();
            absolutes();
            existence();
            mutation();
        } finally {
            rmrf(BASE);
        }
        System.out.println("rows " + rows);
        System.out.println("DONE L4FileSweep");
    }
}
