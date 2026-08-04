import java.io.*;
import java.nio.file.*;
import java.util.*;

public class FilesMatrix {
    static void p(String k, Object v) { System.out.println(k + "\t" + v); }
    static void t(String k, Callable c) { try { p(k, c.run()); } catch (Throwable e) { p(k, "EX " + e.getClass().getSimpleName()); } }
    interface Callable { Object run() throws Exception; }

    public static void main(String[] a) throws Exception {
        Path root = Files.createTempDirectory("fm");
        t("createDirectories(a/b/)", () -> Files.createDirectories(root.resolve("a/b/")).toString().endsWith("a/b"));
        t("isDirectory(a/b/)", () -> Files.isDirectory(root.resolve("a/b/")));
        t("exists(a/b/)", () -> Files.exists(root.resolve("a/b/")));
        t("write file c", () -> Files.writeString(root.resolve("a/b/c"), "hi").toString().endsWith("a/b/c"));
        t("readString(a/b/c)", () -> Files.readString(root.resolve("a/b/c")));
        t("size", () -> Files.size(root.resolve("a/b/c")));
        t("newOutputStream(d/)", () -> { try (OutputStream o = Files.newOutputStream(root.resolve("a/b/d/"))) { o.write(1); } return Files.size(root.resolve("a/b/d")); });
        t("copy c -> e/", () -> Files.copy(root.resolve("a/b/c"), root.resolve("a/b/e/")).toString().endsWith("a/b/e"));
        t("list a/b/", () -> { List<String> n = new ArrayList<>(); try (DirectoryStream<Path> s = Files.newDirectoryStream(root.resolve("a/b/"))) { for (Path q : s) n.add(q.getFileName().toString()); } Collections.sort(n); return n; });
        t("walk count", () -> { try (var st = Files.walk(root)) { return st.count(); } });
        t("delete e/", () -> { Files.delete(root.resolve("a/b/e/")); return Files.exists(root.resolve("a/b/e")); });
        t("createFile f/", () -> Files.createFile(root.resolve("a/b/f/")).toString().endsWith("a/b/f"));
        t("move f/ -> g", () -> Files.move(root.resolve("a/b/f/"), root.resolve("a/b/g")).toString().endsWith("a/b/g"));
        t("readAllLines", () -> Files.readAllLines(root.resolve("a/b/c/".replace("/c/", "/c"))));
        t("relativize", () -> root.relativize(root.resolve("a/b/")).toString());
        t("startsWith", () -> root.resolve("a/b/").startsWith(root));
        t("toRealPath", () -> Files.exists(root.resolve("a/b/").toRealPath()));
        t("attrs isDir", () -> Files.readAttributes(root.resolve("a/b/"), java.nio.file.attribute.BasicFileAttributes.class).isDirectory());
    }
}
