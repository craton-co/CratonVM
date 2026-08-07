import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.attribute.BasicFileAttributes;
import java.nio.file.attribute.FileTime;
import java.util.ArrayList;
import java.util.List;

/**
 * Sweeps the `java.nio.file.Files` static surface that routes through
 * `FileSystemProvider`, printing one deterministic line per call so a CratonVM
 * run can be diffed against a HotSpot run.
 *
 * The point is a defect CLASS, not one method: a `Files` static whose only
 * implementation is an abstract `FileSystemProvider` declaration blows up with
 * `AbstractMethodError ... has no Code attribute`. Anything that answers
 * `AbstractMethodError` here is another instance of it. Written while closing
 * the `Files.setAttribute` instance of that class (2026-08-07), which found
 * four more.
 */
public final class FilesSweep {

    private static final List<String> LINES = new ArrayList<>();

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("filessweep");
        Path f = dir.resolve("a.txt");
        Files.write(f, "hello".getBytes(StandardCharsets.UTF_8));
        Path sub = dir.resolve("sub");
        boolean win = System.getProperty("os.name", "").toLowerCase().contains("win");

        // --- attribute reads ---
        say("size", () -> Files.size(f));
        say("exists", () -> Files.exists(f));
        say("isDirectory", () -> Files.isDirectory(dir));
        say("isRegularFile", () -> Files.isRegularFile(f));
        say("isHidden", () -> Files.isHidden(f));
        say("isSameFile", () -> Files.isSameFile(f, f));
        say("readAttributes(Class)", () -> Files.readAttributes(f, BasicFileAttributes.class).size());
        say("readAttributes(\"basic:*\").size", () -> Files.readAttributes(f, "basic:*").size());
        say("readAttributes(\"size\")", () -> Files.readAttributes(f, "size").get("size"));
        say("getAttribute(basic:size)", () -> Files.getAttribute(f, "basic:size"));
        say("getFileAttributeView(basic)!=null",
                () -> Files.getFileAttributeView(f, java.nio.file.attribute.BasicFileAttributeView.class) != null);
        say("getFileStore!=null", () -> Files.getFileStore(f) != null);
        say("getLastModifiedTime!=null", () -> Files.getLastModifiedTime(f) != null);
        say("probeContentType", () -> String.valueOf(Files.probeContentType(f)));
        say("isReadable", () -> Files.isReadable(f));
        say("isWritable", () -> Files.isWritable(f));
        say("isExecutable", () -> Files.isExecutable(f));

        // --- attribute writes (the doc's defect) ---
        FileTime want = FileTime.fromMillis(1234567890000L);
        say("setAttribute(basic:lastModifiedTime)", () -> {
            Files.setAttribute(f, "basic:lastModifiedTime", want);
            return Files.getLastModifiedTime(f).toMillis();
        });
        say("setLastModifiedTime", () -> {
            Files.setLastModifiedTime(f, FileTime.fromMillis(1000000000000L));
            return Files.getLastModifiedTime(f).toMillis();
        });
        say("setAttribute(lastAccessTime)", () -> {
            Files.setAttribute(f, "lastAccessTime", want);
            return "void";
        });
        say("setAttribute(bogus:x) throws UOE", () -> {
            Files.setAttribute(f, "bogus:x", "v");
            return "NO THROW";
        });
        say("setAttribute(basic:size) throws IAE", () -> {
            Files.setAttribute(f, "basic:size", 5L);
            return "NO THROW";
        });
        if (win) {
            say("setAttribute(dos:readonly=true)", () -> {
                Files.setAttribute(f, "dos:readonly", Boolean.TRUE);
                return Files.getAttribute(f, "dos:readonly");
            });
            say("setAttribute(dos:readonly=false)", () -> {
                Files.setAttribute(f, "dos:readonly", Boolean.FALSE);
                return Files.getAttribute(f, "dos:readonly");
            });
            say("setAttribute(dos:hidden=true)", () -> {
                Files.setAttribute(f, "dos:hidden", Boolean.TRUE);
                return Files.getAttribute(f, "dos:hidden");
            });
            say("setAttribute(dos:hidden=false)", () -> {
                Files.setAttribute(f, "dos:hidden", Boolean.FALSE);
                return Files.getAttribute(f, "dos:hidden");
            });
            say("setAttribute(dos:readonly, \"x\") throws CCE", () -> {
                Files.setAttribute(f, "dos:readonly", "x");
                return "NO THROW";
            });
            say("setAttribute(posix:permissions) throws UOE", () -> {
                Files.setAttribute(f, "posix:permissions",
                        java.nio.file.attribute.PosixFilePermissions.fromString("rw-r--r--"));
                return "NO THROW";
            });
        } else {
            say("setAttribute(posix:permissions)", () -> {
                Files.setAttribute(f, "posix:permissions",
                        java.nio.file.attribute.PosixFilePermissions.fromString("rw-r--r--"));
                return java.nio.file.attribute.PosixFilePermissions
                        .toString(Files.getPosixFilePermissions(f));
            });
        }
        say("setAttribute(missing file) throws NoSuchFile", () -> {
            Files.setAttribute(dir.resolve("nope"), "basic:lastModifiedTime", want);
            return "NO THROW";
        });

        // --- mutations that route through the provider ---
        say("createDirectory", () -> {
            Files.createDirectory(sub);
            return Files.isDirectory(sub);
        });
        say("createDirectories", () -> {
            Files.createDirectories(sub.resolve("x/y"));
            return Files.isDirectory(sub.resolve("x/y"));
        });
        Path copy = dir.resolve("b.txt");
        say("copy", () -> {
            Files.copy(f, copy, StandardCopyOption.REPLACE_EXISTING);
            return Files.size(copy);
        });
        Path moved = dir.resolve("c.txt");
        say("move", () -> {
            Files.move(copy, moved, StandardCopyOption.REPLACE_EXISTING);
            return Files.exists(moved) + "/" + Files.exists(copy);
        });
        say("delete", () -> {
            Files.delete(moved);
            return Files.exists(moved);
        });
        say("deleteIfExists(absent)", () -> Files.deleteIfExists(dir.resolve("nope")));
        say("newDirectoryStream count", () -> {
            int n = 0;
            try (java.nio.file.DirectoryStream<Path> ds = Files.newDirectoryStream(dir)) {
                for (Path ignored : ds) {
                    n++;
                }
            }
            return n;
        });
        say("list count", () -> {
            try (java.util.stream.Stream<Path> s = Files.list(dir)) {
                return s.count();
            }
        });
        say("walk count", () -> {
            try (java.util.stream.Stream<Path> s = Files.walk(dir)) {
                return s.count();
            }
        });
        say("readAllBytes", () -> new String(Files.readAllBytes(f), StandardCharsets.UTF_8));
        say("readString", () -> Files.readString(f));
        say("newInputStream first byte", () -> {
            try (java.io.InputStream in = Files.newInputStream(f)) {
                return in.read();
            }
        });
        say("newOutputStream+read back", () -> {
            try (java.io.OutputStream out = Files.newOutputStream(f)) {
                out.write("bye".getBytes(StandardCharsets.UTF_8));
            }
            return Files.readString(f);
        });
        say("newByteChannel size", () -> {
            try (java.nio.channels.SeekableByteChannel ch = Files.newByteChannel(f)) {
                return ch.size();
            }
        });

        for (String line : LINES) {
            System.out.println(line);
        }
        System.out.println("SWEEP-END n=" + LINES.size());
    }

    private interface Call {
        Object run() throws Throwable;
    }

    /** One line per call: value on success, exception SIMPLE NAME on failure. */
    private static void say(String label, Call c) {
        String result;
        try {
            result = "= " + c.run();
        } catch (Throwable t) {
            result = "! " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : " :: " + firstLine(t.getMessage()));
        }
        LINES.add(pad(label) + result);
    }

    private static String firstLine(String s) {
        int i = s.indexOf('\n');
        return i < 0 ? s : s.substring(0, i);
    }

    private static String pad(String s) {
        StringBuilder b = new StringBuilder(s);
        while (b.length() < 42) {
            b.append(' ');
        }
        return b.append("  ").toString();
    }

    private FilesSweep() {
    }
}
