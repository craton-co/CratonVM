import java.io.*;
import java.nio.ByteBuffer;
import java.nio.channels.SeekableByteChannel;
import java.nio.file.*;
import java.nio.file.attribute.BasicFileAttributes;

/** Throwaway diagnostic for the two rows that did not move. */
public class L4Diag {
    static void t(String tag, R r) {
        try { r.run(); System.out.println(tag + " |no-throw|"); }
        catch (Throwable e) { System.out.println(tag + " |THREW " + e.getClass().getName() + ": " + e.getMessage() + "|"); }
    }
    interface R { void run() throws Throwable; }

    public static void main(String[] a) throws Exception {
        Path base = Paths.get("l4diag");
        Files.createDirectories(base);
        Path missing = base.resolve("nope-dir");
        System.out.println("missing exists |" + Files.exists(missing) + "|");
        System.out.println("missing toString |" + missing + "|");
        t("walkFileTree missing (SimpleFileVisitor)",
          () -> Files.walkFileTree(missing, new SimpleFileVisitor<Path>() {}));
        t("walkFileTree missing (counting visitor)", () -> Files.walkFileTree(missing,
            new SimpleFileVisitor<Path>() {
                public FileVisitResult visitFile(Path p, BasicFileAttributes at) {
                    System.out.println("  visitFile " + p);
                    return FileVisitResult.CONTINUE;
                }
                public FileVisitResult visitFileFailed(Path p, IOException e) throws IOException {
                    System.out.println("  visitFileFailed " + p + " " + e.getClass().getName());
                    throw e;
                }
            }));
        t("walkFileTree 4-arg missing", () -> Files.walkFileTree(missing,
            java.util.Collections.emptySet(), Integer.MAX_VALUE, new SimpleFileVisitor<Path>() {}));

        Path c = base.resolve("chan.bin");
        Files.write(c, new byte[]{1, 2, 3, 4});
        try (SeekableByteChannel ch = Files.newByteChannel(c, StandardOpenOption.WRITE)) {
            System.out.println("channel class |" + ch.getClass().getName() + "|");
            t("truncate(-1)", () -> ch.truncate(-1));
            t("position(-1)", () -> ch.position(-1));
        }
        try (java.nio.channels.FileChannel fc = java.nio.channels.FileChannel.open(c, StandardOpenOption.WRITE)) {
            System.out.println("filechannel class |" + fc.getClass().getName() + "|");
            t("fc truncate(-1)", () -> fc.truncate(-1));
        }
        // teardown
        Files.walk(base).sorted(java.util.Comparator.reverseOrder()).forEach(p -> {
            try { Files.deleteIfExists(p); } catch (IOException e) { }
        });
        System.out.println("DONE L4Diag");
    }
}
