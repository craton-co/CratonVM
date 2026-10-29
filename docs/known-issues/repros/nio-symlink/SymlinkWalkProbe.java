import java.io.IOException;
import java.nio.file.*;
import java.nio.file.attribute.BasicFileAttributes;
import java.util.*;

/**
 * Second half of the symlink probe: the parts of java.nio.file that have to
 * make a follow-vs-don't-follow decision. Every check prints
 * "NAME = <value>" so a CratonVM run can be diffed line-for-line against a
 * real-JDK run of the same probe.
 */
public class SymlinkWalkProbe {

    static void show(String name, Object v) {
        System.out.println(name + " = " + v);
    }

    static void guarded(String name, Callable c) {
        try {
            show(name, c.call());
        } catch (Throwable t) {
            show(name, "THREW " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    interface Callable { Object call() throws Exception; }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("symwalk");

        // Kubernetes ConfigMap shape:
        //   dir/..data/{a,b/c}      (hidden real content)
        //   dir/a -> ..data/a       (relative link)
        //   dir/b -> ..data/b       (relative link to a directory)
        Path data = dir.resolve("..data");
        Files.createDirectories(data.resolve("b"));
        Files.write(data.resolve("a"), "1".getBytes());
        Files.write(data.resolve("b").resolve("c"), "2".getBytes());
        Files.createSymbolicLink(dir.resolve("a"), Paths.get("..data/a"));
        Files.createSymbolicLink(dir.resolve("b"), Paths.get("..data/b"));

        guarded("walk.default", () -> rel(dir, walk(dir, false)));
        guarded("walk.follow", () -> rel(dir, walk(dir, true)));

        guarded("find.default.regularOrLink", () -> rel(dir, find(dir, false)));
        guarded("find.follow.regularOrLink", () -> rel(dir, find(dir, true)));

        // walkFileTree over a symlink-to-directory root: the JDK reports it as
        // one visitFile, never as a directory.
        guarded("walkFileTree.linkRoot", () -> {
            List<String> log = new ArrayList<>();
            Files.walkFileTree(dir.resolve("b"), new SimpleFileVisitor<Path>() {
                public FileVisitResult preVisitDirectory(Path d, BasicFileAttributes a) {
                    log.add("preDir:" + dir.relativize(d).toString().replace('\\', '/'));
                    return FileVisitResult.CONTINUE;
                }
                public FileVisitResult visitFile(Path f, BasicFileAttributes a) {
                    log.add("file:" + dir.relativize(f).toString().replace('\\', '/'));
                    return FileVisitResult.CONTINUE;
                }
                public FileVisitResult postVisitDirectory(Path d, IOException e) {
                    log.add("postDir:" + dir.relativize(d).toString().replace('\\', '/'));
                    return FileVisitResult.CONTINUE;
                }
            });
            return log;
        });

        // Delete a link-to-directory: must unlink, never touch the target.
        guarded("deleteLinkToDir", () -> {
            Path b = dir.resolve("b");
            Files.delete(b);
            return "linkGone=" + !Files.exists(b, LinkOption.NOFOLLOW_LINKS)
                    + " targetKept=" + Files.isDirectory(data.resolve("b"));
        });
        Files.createSymbolicLink(dir.resolve("b"), Paths.get("..data/b"));

        // LinkOption-sensitive predicates.
        Path a = dir.resolve("a");
        Path b = dir.resolve("b");
        Path broken = dir.resolve("broken");
        Files.createSymbolicLink(broken, Paths.get("nope"));
        show("isDirectory.b.follow", Files.isDirectory(b));
        show("isDirectory.b.nofollow", Files.isDirectory(b, LinkOption.NOFOLLOW_LINKS));
        show("isRegularFile.a.follow", Files.isRegularFile(a));
        show("isRegularFile.a.nofollow", Files.isRegularFile(a, LinkOption.NOFOLLOW_LINKS));
        show("exists.broken.follow", Files.exists(broken));
        show("exists.broken.nofollow", Files.exists(broken, LinkOption.NOFOLLOW_LINKS));
        show("notExists.broken.follow", Files.notExists(broken));
        show("notExists.broken.nofollow", Files.notExists(broken, LinkOption.NOFOLLOW_LINKS));
        guarded("deleteIfExists.broken", () -> Files.deleteIfExists(broken));
        show("brokenGoneAfterDelete", !Files.exists(broken, LinkOption.NOFOLLOW_LINKS));

        // A symlink cycle must not hang or explode: without FOLLOW_LINKS the
        // walk simply reports the links.
        Path loopDir = Files.createDirectory(dir.resolve("loop"));
        Files.createSymbolicLink(loopDir.resolve("self"), Paths.get(".."));
        guarded("walk.default.cycle", () -> rel(dir, walk(loopDir, false)));

        System.out.println("done");
    }

    static List<Path> walk(Path root, boolean follow) throws IOException {
        FileVisitOption[] opts = follow
                ? new FileVisitOption[] { FileVisitOption.FOLLOW_LINKS }
                : new FileVisitOption[0];
        try (java.util.stream.Stream<Path> s = Files.walk(root, opts)) {
            return s.toList();
        }
    }

    static List<Path> find(Path root, boolean follow) throws IOException {
        FileVisitOption[] opts = follow
                ? new FileVisitOption[] { FileVisitOption.FOLLOW_LINKS }
                : new FileVisitOption[0];
        try (java.util.stream.Stream<Path> s = Files.find(root, 100,
                (p, at) -> at.isRegularFile() || at.isSymbolicLink(), opts)) {
            return s.toList();
        }
    }

    static List<String> rel(Path base, List<Path> ps) {
        List<String> out = new ArrayList<>();
        for (Path p : ps) {
            String r = base.relativize(p).toString().replace('\\', '/');
            out.add(r.isEmpty() ? "." : r);
        }
        Collections.sort(out);
        return out;
    }
}
