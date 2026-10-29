import java.io.IOException;
import java.nio.file.*;
import java.nio.file.attribute.BasicFileAttributes;

/** Probe for java.nio.file symlink support: create / read / hard-link / attrs. */
public class SymlinkProbe {

    static void step(String name, Runnable r) {
        try {
            r.run();
            System.out.println("[OK  ] " + name);
        } catch (Throwable t) {
            Throwable c = t;
            if (c instanceof RuntimeException && c.getCause() != null) {
                c = c.getCause();
            }
            System.out.println("[FAIL] " + name + " -> " + c.getClass().getName() + ": " + c.getMessage());
            StackTraceElement[] st = c.getStackTrace();
            for (int i = 0; i < Math.min(4, st.length); i++) {
                System.out.println("         at " + st[i]);
            }
        }
    }

    interface IoRun { void run() throws Exception; }

    static void t(String name, IoRun r) {
        step(name, () -> {
            try {
                r.run();
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        });
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("symprobe");
        System.out.println("dir = " + dir);

        Path target = dir.resolve("target.txt");
        Files.write(target, "hello".getBytes());
        Path targetDir = dir.resolve("targetdir");
        Files.createDirectory(targetDir);
        Files.write(targetDir.resolve("inner.txt"), "inner".getBytes());

        Path link = dir.resolve("link.txt");
        t("createSymbolicLink(file)", () -> {
            Path r = Files.createSymbolicLink(link, target);
            System.out.println("       returned: " + r);
        });

        t("isSymbolicLink(link)", () -> {
            boolean b = Files.isSymbolicLink(link);
            System.out.println("       isSymbolicLink = " + b + " (expect true)");
            if (!b) throw new AssertionError("expected true");
        });

        t("readSymbolicLink(link)", () -> {
            Path r = Files.readSymbolicLink(link);
            System.out.println("       readSymbolicLink = " + r + " (expect " + target + ")");
            if (!r.toString().equals(target.toString())) throw new AssertionError("mismatch: " + r);
        });

        t("read through link", () -> {
            String s = new String(Files.readAllBytes(link));
            System.out.println("       content = " + s + " (expect hello)");
            if (!s.equals("hello")) throw new AssertionError("mismatch");
        });

        t("exists(link) follow", () -> {
            boolean b = Files.exists(link);
            System.out.println("       exists = " + b + " (expect true)");
            if (!b) throw new AssertionError("expected true");
        });

        t("readAttributes NOFOLLOW isSymbolicLink", () -> {
            BasicFileAttributes a = Files.readAttributes(link, BasicFileAttributes.class, LinkOption.NOFOLLOW_LINKS);
            System.out.println("       nofollow.isSymbolicLink = " + a.isSymbolicLink() + " (expect true)"
                    + ", isRegularFile = " + a.isRegularFile() + " (expect false)");
            if (!a.isSymbolicLink()) throw new AssertionError("expected symlink");
        });

        t("readAttributes FOLLOW isRegularFile", () -> {
            BasicFileAttributes a = Files.readAttributes(link, BasicFileAttributes.class);
            System.out.println("       follow.isRegularFile = " + a.isRegularFile() + " (expect true)"
                    + ", isSymbolicLink = " + a.isSymbolicLink() + " (expect false)");
            if (!a.isRegularFile()) throw new AssertionError("expected regular file");
        });

        Path dlink = dir.resolve("dlink");
        t("createSymbolicLink(dir)", () -> Files.createSymbolicLink(dlink, targetDir));
        t("isDirectory(dlink)", () -> {
            boolean b = Files.isDirectory(dlink);
            System.out.println("       isDirectory = " + b + " (expect true)");
            if (!b) throw new AssertionError("expected true");
        });
        t("list through dir link", () -> {
            java.util.List<String> names = new java.util.ArrayList<>();
            try (DirectoryStream<Path> ds = Files.newDirectoryStream(dlink)) {
                for (Path p : ds) names.add(p.getFileName().toString());
            }
            System.out.println("       entries = " + names + " (expect [inner.txt])");
            if (!names.equals(java.util.List.of("inner.txt"))) throw new AssertionError("mismatch");
        });

        // Relative symlink: link2 -> "target.txt" (relative to dir)
        Path rel = dir.resolve("rel.txt");
        t("createSymbolicLink(relative target)", () -> Files.createSymbolicLink(rel, Paths.get("target.txt")));
        t("readSymbolicLink(relative) stays relative", () -> {
            Path r = Files.readSymbolicLink(rel);
            System.out.println("       = " + r + " (expect target.txt)");
            if (!r.toString().equals("target.txt")) throw new AssertionError("mismatch: " + r);
        });
        t("read through relative link", () -> {
            String s = new String(Files.readAllBytes(rel));
            if (!s.equals("hello")) throw new AssertionError("mismatch: " + s);
        });

        // Existing link -> FileAlreadyExistsException
        t("createSymbolicLink over existing -> FileAlreadyExistsException", () -> {
            try {
                Files.createSymbolicLink(link, target);
                throw new AssertionError("expected FileAlreadyExistsException");
            } catch (FileAlreadyExistsException expected) {
                System.out.println("       got FileAlreadyExistsException: " + expected.getMessage());
            }
        });

        // readSymbolicLink on a non-link -> NotLinkException
        t("readSymbolicLink(non-link) -> NotLinkException", () -> {
            try {
                Path r = Files.readSymbolicLink(target);
                throw new AssertionError("expected NotLinkException, got " + r);
            } catch (NotLinkException expected) {
                System.out.println("       got NotLinkException: " + expected.getMessage());
            }
        });

        // hard link
        Path hard = dir.resolve("hard.txt");
        t("createLink(hard)", () -> {
            Path r = Files.createLink(hard, target);
            System.out.println("       returned: " + r);
        });
        t("hard link content + not a symlink", () -> {
            String s = new String(Files.readAllBytes(hard));
            boolean sym = Files.isSymbolicLink(hard);
            System.out.println("       content = " + s + ", isSymbolicLink = " + sym + " (expect hello,false)");
            if (!s.equals("hello") || sym) throw new AssertionError("mismatch");
        });

        // delete a link should remove the link, not the target
        t("delete(link) keeps target", () -> {
            Files.delete(link);
            boolean linkGone = !Files.exists(link, LinkOption.NOFOLLOW_LINKS);
            boolean targetKept = Files.exists(target);
            System.out.println("       linkGone = " + linkGone + ", targetKept = " + targetKept + " (expect true,true)");
            if (!linkGone || !targetKept) throw new AssertionError("mismatch");
        });

        // broken symlink
        Path broken = dir.resolve("broken");
        t("broken symlink semantics", () -> {
            Files.createSymbolicLink(broken, dir.resolve("does-not-exist"));
            boolean e = Files.exists(broken);
            boolean eN = Files.exists(broken, LinkOption.NOFOLLOW_LINKS);
            boolean s = Files.isSymbolicLink(broken);
            System.out.println("       exists=" + e + " existsNOFOLLOW=" + eN + " isSymbolicLink=" + s
                    + " (expect false,true,true)");
            if (e || !eN || !s) throw new AssertionError("mismatch");
        });

        // walk a tree containing a symlink in the path (ConfigTree shape)
        t("nested dir symlink walk", () -> {
            Path a = dir.resolve("cfg");
            Files.createDirectories(a.resolve("..data/x"));
            Files.write(a.resolve("..data/x/k"), "v".getBytes());
            Files.createSymbolicLink(a.resolve("x"), a.resolve("..data/x"));
            java.util.List<String> found = new java.util.ArrayList<>();
            try (java.util.stream.Stream<Path> st = Files.find(a, 100,
                    (p, at) -> at.isRegularFile())) {
                st.forEach(p -> found.add(a.relativize(p).toString().replace('\\', '/')));
            }
            java.util.Collections.sort(found);
            System.out.println("       found = " + found);
            if (!found.contains("..data/x/k")) throw new AssertionError("mismatch");
        });

        System.out.println("done");
    }
}
