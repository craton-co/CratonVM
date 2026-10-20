import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.nio.file.attribute.*;
import java.util.*;
import java.util.stream.*;

/**
 * The four class-identity divergences `WORKER-4-1` N3 left as nominations, asked
 * BEHAVIOURALLY rather than by `getClass()`.
 *
 * ```text
 *                                          CratonVM                      HotSpot
 *   Files.newInputStream(f)                java.io.FileInputStream       sun.nio.ch.ChannelInputStream
 *   Files.newOutputStream(f)               java.io.FileOutputStream      sun.nio.ch.ChannelOutputStream
 *   Files.walk(dir)                        ReferencePipeline$Head        ReferencePipeline$3
 *   Files.readAttributes(f, Basic.class)   UnixFileAttributes            …$UnixAsBasicFileAttributes
 * ```
 *
 * A class-identity difference is not automatically a defect — `WORKER-4-1` said
 * so and left them. But each of these four has a REACHABLE consequence, and
 * this probe is the difference between "the names differ" and "an application
 * can tell":
 *
 *   * `instanceof` on the returned object takes a different branch — and
 *     `if (in instanceof FileInputStream fis) fis.getChannel()` is a real
 *     idiom;
 *   * `readAttributes(p, BasicFileAttributes.class)` returning the FULL
 *     `UnixFileAttributes` also answers `instanceof PosixFileAttributes`, so a
 *     caller can reach `permissions()` off a BASIC view;
 *   * a `Files.walk` stream must be CLOSEABLE and its close must run the
 *     handler that releases the directory stream.
 */
public class W4Files {

    static Path root;

    static void ck(String tag, Object got) {
        String s = String.valueOf(got);
        if (root != null) {
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

    public static void main(String[] args) throws Exception {
        root = Files.createTempDirectory("w4files");
        Path f = root.resolve("data.txt");
        Files.write(f, "0123456789".getBytes(StandardCharsets.UTF_8));
        Path sub = Files.createDirectory(root.resolve("sub"));
        Files.write(sub.resolve("nested.txt"), "n".getBytes(StandardCharsets.UTF_8));

        // ---- Files.newInputStream: what can a caller TELL? ---------------
        try (InputStream in = Files.newInputStream(f)) {
            ck("nis.isFileInputStream", in instanceof FileInputStream);
            ck("nis.isFilterInputStream", in instanceof FilterInputStream);
            ck("nis.markSupported", in.markSupported());
            ck("nis.available", in.available());
            ck("nis.read", in.read());
            ck("nis.skip", in.skip(3));
            ck("nis.readAllBytes", new String(in.readAllBytes(), StandardCharsets.UTF_8));
            ck("nis.readAtEof", in.read());
        }
        // `getChannel()` is the reason `instanceof FileInputStream` matters.
        ckT("nis.getChannelIfFIS", () -> {
            try (InputStream in = Files.newInputStream(f)) {
                if (in instanceof FileInputStream fis) {
                    return "channel-size=" + fis.getChannel().size();
                }
                return "not-a-FileInputStream";
            }
        });
        // A stream over a directory must fail, and the failure type is fixed.
        ckT("nis.onDirectory", () -> Files.newInputStream(sub).read());
        ckT("nis.missing", () -> Files.newInputStream(root.resolve("nope")));
        // Reading through the returned stream after close is a fixed failure.
        ckT("nis.readAfterClose", () -> {
            InputStream in = Files.newInputStream(f);
            in.close();
            return in.read();
        });

        // ---- Files.newOutputStream --------------------------------------
        Path out = root.resolve("out.txt");
        try (OutputStream o = Files.newOutputStream(out)) {
            ck("nos.isFileOutputStream", o instanceof FileOutputStream);
            o.write("first".getBytes(StandardCharsets.UTF_8));
        }
        ck("nos.wrote", new String(Files.readAllBytes(out), StandardCharsets.UTF_8));
        try (OutputStream o = Files.newOutputStream(out, StandardOpenOption.APPEND)) {
            o.write("+app".getBytes(StandardCharsets.UTF_8));
        }
        ck("nos.appended", new String(Files.readAllBytes(out), StandardCharsets.UTF_8));
        try (OutputStream o = Files.newOutputStream(out)) {
            o.write("trunc".getBytes(StandardCharsets.UTF_8));
        }
        ck("nos.truncated", new String(Files.readAllBytes(out), StandardCharsets.UTF_8));
        ckT("nos.createNewExisting", () ->
                Files.newOutputStream(out, StandardOpenOption.CREATE_NEW));
        ckT("nos.readAfterClose", () -> {
            OutputStream o = Files.newOutputStream(root.resolve("closed.txt"));
            o.close();
            o.write(1);
            return "no-throw";
        });

        // ---- Files.walk: the stream must be closeable and RUN its handler --
        try (Stream<Path> w = Files.walk(root)) {
            ck("walk.count", w.count());
        }
        try (Stream<Path> w = Files.walk(root)) {
            ck("walk.names", w.map(p -> root.relativize(p).toString())
                    .filter(x -> !x.isEmpty()).sorted().collect(Collectors.toList()));
        }
        try (Stream<Path> w = Files.walk(root, 1)) {
            ck("walk.depth1", w.count());
        }
        final boolean[] closed = {false};
        try (Stream<Path> w = Files.walk(root).onClose(() -> closed[0] = true)) {
            w.findFirst();
        }
        ck("walk.onCloseRan", closed[0]);
        ckT("walk.missing", () -> Files.walk(root.resolve("nope")).count());
        // `Files.list` is the shallow sibling and has the same shape.
        try (Stream<String> ln = Files.lines(f)) {
            ck("lines.count", ln.count());
        }
        try (Stream<Path> l = Files.list(root)) {
            ck("list.count", l.count());
        }
        final boolean[] listClosed = {false};
        try (Stream<Path> l = Files.list(root).onClose(() -> listClosed[0] = true)) {
            l.findFirst();
        }
        ck("list.onCloseRan", listClosed[0]);

        // ---- Files.readAttributes: which VIEW came back? -------------------
        BasicFileAttributes basic = Files.readAttributes(f, BasicFileAttributes.class);
        ck("attrs.isRegularFile", basic.isRegularFile());
        ck("attrs.isDirectory", basic.isDirectory());
        ck("attrs.isSymbolicLink", basic.isSymbolicLink());
        ck("attrs.isOther", basic.isOther());
        ck("attrs.size", basic.size());
        ck("attrs.hasFileKey", basic.fileKey() != null);
        // THE ONE THAT MATTERS: a BASIC view must not also be a POSIX view.
        ck("attrs.basicIsAlsoPosix", basic instanceof PosixFileAttributes);
        ckT("attrs.reachPermissionsFromBasic", () -> {
            if (basic instanceof PosixFileAttributes p) {
                return "reachable:" + (p.permissions() != null);
            }
            return "not-reachable";
        });
        BasicFileAttributes dirAttrs = Files.readAttributes(sub, BasicFileAttributes.class);
        ck("attrs.dir.isDirectory", dirAttrs.isDirectory());
        ck("attrs.dir.isRegularFile", dirAttrs.isRegularFile());
        ckT("attrs.missing", () ->
                Files.readAttributes(root.resolve("nope"), BasicFileAttributes.class));
        ck("attrs.timesOrdered",
                basic.lastModifiedTime().toMillis() >= 0
                        && basic.creationTime().toMillis() >= 0);
        // The map form, which names the attributes rather than typing them.
        ck("attrs.map.keys", new TreeSet<>(
                Files.readAttributes(f, "basic:size,isRegularFile").keySet()));
        ck("attrs.map.size", Files.readAttributes(f, "basic:size").get("size"));

        System.out.println("PASS W4Files");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
