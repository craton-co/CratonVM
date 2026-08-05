import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.Writer;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.channels.SeekableByteChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.OpenOption;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.EnumSet;
import java.util.HashSet;
import java.util.Set;

/**
 * Regression: nio-write-ignores-nofollow-links-symlink-20260804.
 *
 * `LinkOption.NOFOLLOW_LINKS` implements `OpenOption`, so it is legal in every
 * `Files.write*` / `new*Stream` / `open` varargs list. When the FINAL component
 * of the path is itself a symbolic link, the platform providers add `O_NOFOLLOW`
 * to the open flags and the kernel refuses with `ELOOP` — surfaced as an
 * `IOException` — before anything is created or truncated.
 *
 * CratonVM's open paths scanned the option list for `APPEND` and `CREATE_NEW`
 * only. `NOFOLLOW_LINKS` was never inspected, so every one of these calls
 * followed the link and wrote through to its target: silently, successfully,
 * and to a file the caller had explicitly asked never to touch. Found via
 * `org.springframework.boot.system.ApplicationPidTests`, whose
 * `ApplicationPid.write` uses exactly this option so a PID file swapped for a
 * symlink cannot be used to clobber the link's target.
 *
 * Every refusal below is paired with a control that must still SUCCEED, so a
 * blanket "always throw on a symlink" regression cannot pass this test:
 * dropping NOFOLLOW_LINKS must write through the link, NOFOLLOW_LINKS on an
 * ordinary file must open normally, and a symlinked DIRECTORY in the middle of
 * the path is not the final component and so is not the option's business.
 */
public class RNioNoFollow {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    interface Body {
        void run() throws Exception;
    }

    /** "io" when the body raised an IOException, else a token naming what it did instead. */
    static String outcome(Body body) {
        try {
            body.run();
            return "no-exception";
        } catch (IOException expected) {
            return "io";
        } catch (Exception other) {
            return "wrong-type:" + other.getClass().getName();
        }
    }

    static Path dir;

    static Path target(String name) {
        return dir.resolve(name + ".target");
    }

    /** `<name>` as a symbolic link to `<name>.target`, which is created with "target". */
    static Path link(String name, boolean targetExists) throws IOException {
        Path t = target(name);
        if (targetExists && !Files.exists(t)) {
            Files.write(t, "target".getBytes(StandardCharsets.UTF_8), StandardOpenOption.CREATE_NEW);
        }
        Path l = dir.resolve(name);
        Files.createSymbolicLink(l, t);
        return l;
    }

    static String content(Path p) {
        try {
            return Files.readString(p);
        } catch (IOException ex) {
            return "<unreadable>";
        }
    }

    public static void main(String[] args) throws Exception {
        dir = Files.createTempDirectory("rnionofollow");
        // Creating a symbolic link needs a privilege Windows does not grant by
        // default. Report that as its own deterministic line rather than
        // pretending the vectors ran: both VMs print it, so the cross-VM diff
        // still matches, and on Linux (where the bug was found) everything below
        // genuinely executes.
        try {
            Files.createSymbolicLink(dir.resolve("probe"), dir.resolve("probe.target"));
        } catch (Exception unsupported) {
            System.out.println("CK RNioNoFollow symlinks=unavailable");
            System.out.println("PASS RNioNoFollow");
            return;
        }

        // --- the refusals -------------------------------------------------
        String writeString = outcome(() -> Files.writeString(link("a", true), "123",
                StandardOpenOption.TRUNCATE_EXISTING, StandardOpenOption.CREATE,
                LinkOption.NOFOLLOW_LINKS));
        check(writeString.equals("io"), "writeString through a symlink: " + writeString);
        check(content(target("a")).equals("target"), "writeString must not touch the target");

        // A DANGLING link still ELOOPs, and must not create the target behind it.
        String dangling = outcome(() -> Files.writeString(link("b", false), "123",
                StandardOpenOption.CREATE, LinkOption.NOFOLLOW_LINKS));
        check(dangling.equals("io"), "writeString through a dangling symlink: " + dangling);
        check(!Files.exists(target("b")), "a refused write must not create the link target");

        String outStream = outcome(() -> {
            try (OutputStream out = Files.newOutputStream(link("c", true),
                    StandardOpenOption.CREATE, StandardOpenOption.WRITE,
                    LinkOption.NOFOLLOW_LINKS)) {
                out.write('x');
            }
        });
        check(outStream.equals("io"), "newOutputStream through a symlink: " + outStream);
        check(content(target("c")).equals("target"), "newOutputStream must not touch the target");

        String inStream = outcome(() -> {
            try (InputStream in = Files.newInputStream(link("d", true),
                    LinkOption.NOFOLLOW_LINKS)) {
                in.read();
            }
        });
        check(inStream.equals("io"), "newInputStream through a symlink: " + inStream);

        String byteChannel = outcome(() -> {
            Set<OpenOption> opts = new HashSet<>(
                    EnumSet.of(StandardOpenOption.WRITE, StandardOpenOption.CREATE));
            opts.add(LinkOption.NOFOLLOW_LINKS);
            try (SeekableByteChannel ch = Files.newByteChannel(link("e", true), opts)) {
                ch.write(ByteBuffer.wrap(new byte[] { 'x' }));
            }
        });
        check(byteChannel.equals("io"), "newByteChannel through a symlink: " + byteChannel);
        check(content(target("e")).equals("target"), "newByteChannel must not touch the target");

        // A READ-only open of a DANGLING link: `Path.exists()` is a stat and
        // reports it absent, so a missing-file pre-check would answer
        // NoSuchFileException (which callers catch and recover from) where the
        // kernel's O_NOFOLLOW answers ELOOP. Both are IOExceptions, so assert
        // the TYPE, not just that something was thrown.
        Path dead = link("i", false);
        String deadType = "none";
        try (SeekableByteChannel ch = Files.newByteChannel(dead,
                Set.of(StandardOpenOption.READ, LinkOption.NOFOLLOW_LINKS))) {
            ch.position();
        } catch (Exception ex) {
            deadType = ex.getClass().getName();
        }
        checks++;
        System.out.println("CK RNioNoFollow danglingReadOnly=" + deadType);

        String fileChannel = outcome(() -> {
            try (FileChannel ch = FileChannel.open(link("f", true), StandardOpenOption.WRITE,
                    StandardOpenOption.CREATE, LinkOption.NOFOLLOW_LINKS)) {
                ch.write(ByteBuffer.wrap(new byte[] { 'x' }));
            }
        });
        check(fileChannel.equals("io"), "FileChannel.open through a symlink: " + fileChannel);
        check(content(target("f")).equals("target"), "FileChannel.open must not touch the target");

        String bufWriter = outcome(() -> {
            try (Writer w = Files.newBufferedWriter(link("g", true), StandardCharsets.UTF_8,
                    StandardOpenOption.CREATE, StandardOpenOption.WRITE,
                    LinkOption.NOFOLLOW_LINKS)) {
                w.write("x");
            }
        });
        check(bufWriter.equals("io"), "newBufferedWriter through a symlink: " + bufWriter);
        check(content(target("g")).equals("target"), "newBufferedWriter must not touch the target");

        // --- the controls that must still succeed --------------------------
        Path follow = link("h", true);
        Files.writeString(follow, "written", StandardOpenOption.TRUNCATE_EXISTING,
                StandardOpenOption.CREATE);
        check(content(target("h")).equals("written"), "without NOFOLLOW_LINKS the write follows");
        check(Files.isSymbolicLink(follow), "the link itself must survive the write");

        Path plain = dir.resolve("plain");
        Files.writeString(plain, "plain-1", StandardOpenOption.CREATE, LinkOption.NOFOLLOW_LINKS);
        check(content(plain).equals("plain-1"), "NOFOLLOW_LINKS on a regular file must write");
        try (InputStream in = Files.newInputStream(plain, LinkOption.NOFOLLOW_LINKS)) {
            check(in.read() == 'p', "NOFOLLOW_LINKS on a regular file must read");
        }
        try (FileChannel ch = FileChannel.open(plain, StandardOpenOption.READ,
                LinkOption.NOFOLLOW_LINKS)) {
            check(ch.size() == 7, "NOFOLLOW_LINKS on a regular file must open a channel");
        }

        // Only the FINAL component is the option's business: a symlinked
        // directory earlier in the path is followed as normal.
        Path realDir = Files.createDirectory(dir.resolve("realdir"));
        Path dirLink = dir.resolve("dirlink");
        Files.createSymbolicLink(dirLink, realDir);
        Files.writeString(dirLink.resolve("inner"), "inner", StandardOpenOption.CREATE,
                LinkOption.NOFOLLOW_LINKS);
        check(content(realDir.resolve("inner")).equals("inner"),
                "NOFOLLOW_LINKS must only inspect the final path component");

        // APPEND and CREATE_NEW travel the same scanner as NOFOLLOW_LINKS, and
        // were the only two options it ever read — assert they still work.
        Path app = dir.resolve("appended");
        Files.writeString(app, "one", StandardOpenOption.CREATE);
        Files.writeString(app, "-two", StandardOpenOption.APPEND);
        check(content(app).equals("one-two"), "APPEND must append, not truncate: " + content(app));
        String createNew = outcome(
                () -> Files.writeString(app, "clobber", StandardOpenOption.CREATE_NEW));
        check(createNew.equals("io"), "CREATE_NEW on an existing file: " + createNew);
        check(content(app).equals("one-two"), "a refused CREATE_NEW must not have written");

        // The Files.write* statics now open through the fd table and write via a
        // buffered writer rather than a single std::fs::write. Assert the three
        // things that can silently go wrong with that: a payload larger than any
        // internal buffer must round-trip whole, a shorter write over a longer
        // file must TRUNCATE rather than leave a tail, and the Iterable overload
        // must still emit one newline-terminated line per element.
        Path big = dir.resolve("big");
        byte[] payload = new byte[4 * 1024 * 1024 + 7];
        for (int i = 0; i < payload.length; i++) {
            payload[i] = (byte) (i * 31 + 7);
        }
        Files.write(big, payload);
        byte[] readBack = Files.readAllBytes(big);
        check(readBack.length == payload.length,
                "large write round-trip length: " + readBack.length + " != " + payload.length);
        check(java.util.Arrays.equals(readBack, payload), "large write round-trip content");
        Files.write(big, new byte[] { 'a', 'b', 'c' });
        check(Files.size(big) == 3, "a shorter write must truncate, size=" + Files.size(big));

        Path lines = dir.resolve("lines");
        Files.write(lines, java.util.List.of("alpha", "beta"));
        check(content(lines).equals("alpha\nbeta\n"), "Iterable write: " + content(lines));

        System.out.println("CK RNioNoFollow checks=" + checks);
        System.out.println("CK RNioNoFollow refusals=" + writeString + "," + dangling + ","
                + outStream + "," + inStream + "," + byteChannel + "," + fileChannel + ","
                + bufWriter + "," + createNew);
        System.out.println("PASS RNioNoFollow");
    }
}
