import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * `java.io.File` — 54 owned §1.4 shadow rows, the largest single class in the
 * `java.io` census (`WORKER-4-2` §1.1), and none of them checked against the
 * oracle for anything but "does not throw".
 *
 * Every line prints a value the javadoc fixes, so the oracle and the VM must
 * agree exactly. Paths are made relative to a temp directory and echoed with
 * the directory prefix stripped, so the output is comparable between runs.
 */
public class W4File {

    static Path root;

    static void ck(String tag, Object got) {
        String s = String.valueOf(got);
        if (root != null) {
            // The temp directory's own basename carries a random suffix, so it
            // must be masked as well as the path that contains it — otherwise
            // every run diffs against every other run and the real differences
            // are buried.
            s = s.replace(root.toString(), "<ROOT>")
                 .replace(root.getFileName().toString(), "<ROOTNAME>")
                 .replace('\\', '/');
        }
        System.out.println("CK " + tag + " " + s);
    }

    interface Thunk { Object call() throws Exception; }

    static void ckT(String tag, Thunk t) {
        try {
            ck(tag, t.call());
        } catch (Throwable e) {
            ck(tag, "threw:" + e.getClass().getName());
        }
    }

    static String sortedList(String[] a) {
        if (a == null) return "null";
        String[] c = a.clone();
        Arrays.sort(c);
        return Arrays.toString(c);
    }

    static String sortedFiles(File[] a) {
        if (a == null) return "null";
        String[] names = new String[a.length];
        for (int i = 0; i < a.length; i++) names[i] = a[i].getName();
        Arrays.sort(names);
        return Arrays.toString(names);
    }

    public static void main(String[] args) throws Exception {
        root = Files.createTempDirectory("w4file");
        File dir = root.toFile();
        File a = new File(dir, "alpha.txt");
        Files.write(a.toPath(), "0123456789".getBytes(StandardCharsets.UTF_8));
        File sub = new File(dir, "sub");
        sub.mkdir();
        File nested = new File(sub, "beta.txt");
        Files.write(nested.toPath(), "xy".getBytes(StandardCharsets.UTF_8));
        File missing = new File(dir, "nope.txt");

        // ---- naming and path decomposition ------------------------------
        ck("getName", a.getName());
        ck("getName.dir", sub.getName());
        ck("getParent", a.getParent());
        ck("getParentFile.getName", a.getParentFile().getName());
        ck("isAbsolute", a.isAbsolute());
        ck("relative.isAbsolute", new File("rel/ative.txt").isAbsolute());
        ck("relative.getParent", new File("rel/ative.txt").getParent());
        ck("relative.getName", new File("rel/ative.txt").getName());
        ck("noParent.getParent", new File("solo.txt").getParent());
        ck("trailingSlash.getName", new File("a/b/").getName());
        ck("doubleSlash.getPath", new File("a//b").getPath());
        ck("dotSegments.getPath", new File("a/./b/../c").getPath());
        ck("empty.getName", new File("").getName());
        ck("empty.getPath", new File("").getPath());
        ckT("empty.getAbsolutePath.endsWith", () ->
                new File("").getAbsolutePath().endsWith(System.getProperty("user.dir")));
        ck("twoArg.getPath", new File("par", "chi").getPath());
        ck("twoArgFile.getPath", new File(new File("par"), "chi").getPath());
        ck("twoArgEmptyChild", new File("par", "").getPath());
        ck("separator", File.separator);
        ck("separatorChar", File.separatorChar);
        ck("pathSeparator", File.pathSeparator);

        // ---- existence and kind -----------------------------------------
        ck("exists", a.exists());
        ck("exists.missing", missing.exists());
        ck("isFile", a.isFile());
        ck("isFile.dir", sub.isFile());
        ck("isDirectory", sub.isDirectory());
        ck("isDirectory.file", a.isDirectory());
        ck("isDirectory.missing", missing.isDirectory());
        ck("length", a.length());
        ck("length.missing", missing.length());
        ck("length.dir>=0", sub.length() >= 0);
        ck("canRead", a.canRead());
        ck("canWrite", a.canWrite());
        ck("canRead.missing", missing.canRead());
        ck("isHidden", a.isHidden());
        ck("isHidden.dot", new File(dir, ".hidden").isHidden());

        // ---- listing ------------------------------------------------------
        ck("list", sortedList(dir.list()));
        ck("list.file", sortedList(a.list()));
        ck("list.missing", sortedList(missing.list()));
        ck("listFiles", sortedFiles(dir.listFiles()));
        ck("listFiles.filtered", sortedFiles(dir.listFiles(
                (FileFilter) f -> f.getName().endsWith(".txt"))));
        ck("listFiles.nameFiltered", sortedFiles(dir.listFiles(
                (FilenameFilter) (d, n) -> n.startsWith("s"))));
        ck("list.nameFiltered", sortedList(dir.list((d, n) -> n.startsWith("a"))));

        // ---- comparison and identity --------------------------------------
        ck("equals.same", a.equals(new File(dir, "alpha.txt")));
        ck("equals.other", a.equals(sub));
        ck("hashCode.stable", a.hashCode() == new File(dir, "alpha.txt").hashCode());
        ck("compareTo.self", a.compareTo(new File(dir, "alpha.txt")));
        ck("compareTo.sign", Integer.signum(a.compareTo(sub)));
        ck("toString", a.toString());

        // ---- URI / Path round trips ---------------------------------------
        ck("toURI.scheme", a.toURI().getScheme());
        ck("toURI.roundTrip", new File(a.toURI()).getName());
        ck("toPath.getFileName", a.toPath().getFileName().toString());
        ck("toPath.roundTrip", a.toPath().toFile().getName());
        ck("getAbsoluteFile.getName", a.getAbsoluteFile().getName());
        ckT("getCanonicalPath.endsWith", () -> a.getCanonicalPath().endsWith("alpha.txt"));
        ckT("getCanonicalFile.dots", () ->
                new File(dir, "sub/../alpha.txt").getCanonicalFile().getName());

        // ---- mutation -----------------------------------------------------
        File made = new File(dir, "made.txt");
        ck("createNewFile", made.createNewFile());
        ck("createNewFile.again", made.createNewFile());
        ck("exists.after", made.exists());
        File moved = new File(dir, "moved.txt");
        ck("renameTo", made.renameTo(moved));
        ck("renameTo.srcGone", made.exists());
        ck("renameTo.dstThere", moved.exists());
        ck("setLastModified", moved.setLastModified(1000000000000L));
        ck("lastModified", moved.lastModified());
        ck("setReadOnly", moved.setReadOnly());
        ck("canWrite.afterReadOnly", moved.canWrite());
        ck("setWritable", moved.setWritable(true));
        ck("canWrite.afterWritable", moved.canWrite());
        ck("delete", moved.delete());
        ck("delete.again", moved.delete());
        ck("mkdir.nested.fails", new File(dir, "x/y/z").mkdir());
        ck("mkdirs.nested", new File(dir, "x/y/z").mkdirs());
        ck("mkdirs.exists", new File(dir, "x/y/z").mkdirs());
        ck("mkdir.overExisting", sub.mkdir());

        // ---- disk space (magnitudes, not exact values) ---------------------
        ck("getTotalSpace>0", dir.getTotalSpace() > 0);
        ck("getFreeSpace>0", dir.getFreeSpace() > 0);
        ck("getUsableSpace>0", dir.getUsableSpace() > 0);
        ck("getUsableSpace<=Total", dir.getUsableSpace() <= dir.getTotalSpace());
        ck("getTotalSpace.missing", missing.getTotalSpace());

        // ---- statics --------------------------------------------------------
        ck("listRoots.nonEmpty", File.listRoots().length > 0);
        File tmp = File.createTempFile("w4f", ".tmp", dir);
        ck("createTempFile.exists", tmp.exists());
        ck("createTempFile.prefix", tmp.getName().startsWith("w4f"));
        ck("createTempFile.suffix", tmp.getName().endsWith(".tmp"));
        ck("createTempFile.parent", tmp.getParentFile().getName().equals(dir.getName()));
        tmp.delete();

        // ---- streams over a File --------------------------------------------
        try (FileOutputStream out = new FileOutputStream(new File(dir, "w.txt"))) {
            out.write("written".getBytes(StandardCharsets.UTF_8));
        }
        ck("fos.thenRead", new String(
                Files.readAllBytes(new File(dir, "w.txt").toPath()), StandardCharsets.UTF_8));
        try (FileOutputStream out = new FileOutputStream(new File(dir, "w.txt"), true)) {
            out.write("+more".getBytes(StandardCharsets.UTF_8));
        }
        ck("fos.append", new String(
                Files.readAllBytes(new File(dir, "w.txt").toPath()), StandardCharsets.UTF_8));
        try (FileWriter w = new FileWriter(new File(dir, "fw.txt"))) {
            w.write("filewriter");
        }
        ck("filewriter", new String(
                Files.readAllBytes(new File(dir, "fw.txt").toPath()), StandardCharsets.UTF_8));
        try (RandomAccessFile raf = new RandomAccessFile(a, "r")) {
            ck("raf.length", raf.length());
            byte[] b = new byte[4];
            raf.seek(3);
            raf.readFully(b);
            ck("raf.seekRead", new String(b, StandardCharsets.UTF_8));
            ck("raf.filePointer", raf.getFilePointer());
        }

        System.out.println("PASS W4File");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
