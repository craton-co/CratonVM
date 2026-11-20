import java.io.ByteArrayInputStream;
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
import java.nio.file.StandardCopyOption;
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
        // default.
        //
        // WHAT THIS USED TO DO, AND WHY IT WAS WRONG. It printed
        // `CK RNioNoFollow symlinks=unavailable`, printed PASS, and returned —
        // skipping ALL 27 checks. Both VMs printed the same bail-out line, so
        // the cross-VM diff agreed, the exit code was 0 and the gate was green.
        // run.sh says the suite is usually run from Git Bash on Windows, so on
        // the PRIMARY platform this vector asserted nothing at all, for the
        // entire time it was scheduled. The comment that stood here — "both VMs
        // print it, so the cross-VM diff still matches" — described the defect
        // as though it were the design.
        //
        // The guard was also over-broad twice over. Six of the checks below
        // need no symlink whatever: NOFOLLOW_LINKS on a REGULAR file (three of
        // them), APPEND, CREATE_NEW refusal, the shorter-write truncation, the
        // 4 MiB round trip and Files.write(Iterable). Those travel the same
        // option scanner the defect lived in, and they were being thrown away
        // with the rest.
        //
        // So the bail-out is now scoped to the arms that genuinely need the
        // privilege, and the count of assertions that ACTUALLY RAN is published
        // on the PASS line. That count is the instrument: a VM that bails where
        // the oracle does not reports a different number and the diff goes red,
        // which is precisely what could not happen before.
        boolean symlinks;
        try {
            Files.createSymbolicLink(dir.resolve("probe"), dir.resolve("probe.target"));
            symlinks = true;
        } catch (Exception unsupported) {
            symlinks = false;
        }
        System.out.println("CK RNioNoFollow symlinks=" + (symlinks ? "available" : "unavailable"));

        String refusals = symlinks ? symlinkArms() : "no-symlink-privilege";

        plainFileArms();
        optionListIsRead();

        System.out.println("CK RNioNoFollow refusals=" + refusals);
        System.out.println("CK RNioNoFollow checks=" + checks);
        System.out.println("PASS RNioNoFollow (" + checks + " checks)");
    }

    /**
     * Whether the option list is READ AT ALL — the other half of the defect
     * this vector was filed for, and the half that needs no symlink and no
     * privilege, so unlike every arm below it actually executes on Windows.
     *
     * The scanner that ignored `NOFOLLOW_LINKS` also never rejected an option
     * the JDK refuses outright. Note the two exception types are deliberately
     * DIFFERENT: `newInputStream` raises `UnsupportedOperationException` for a
     * write option, `newOutputStream` raises `IllegalArgumentException` for
     * `READ`. A VM that mapped both onto one class would satisfy neither row.
     *
     * The three trailing `=ok` rows are not decoration: without them a VM that
     * threw on EVERY option would satisfy all three refusals above.
     * `copyStream.REPLACE=ok` matters most — `REPLACE_EXISTING` is the one
     * option the copy path is supposed to accept, and it is what a too-eager
     * refusal would break. See W7-8-fabricated-success-io-sweep.md §8.5.
     */
    static void optionListIsRead() throws Exception {
        Path f = Files.createTempFile("rnio-opt", ".tmp");
        try {
            String in = "none";
            try {
                Files.newInputStream(f, StandardOpenOption.WRITE).close();
            } catch (UnsupportedOperationException e) {
                in = "UnsupportedOperationException";
            } catch (Exception e) {
                in = e.getClass().getSimpleName();
            }
            System.out.println("CK RNioNoFollow newInputStream.WRITE=" + in);

            String out = "none";
            try {
                Files.newOutputStream(f, StandardOpenOption.READ).close();
            } catch (IllegalArgumentException e) {
                out = "IllegalArgumentException";
            } catch (Exception e) {
                out = e.getClass().getSimpleName();
            }
            System.out.println("CK RNioNoFollow newOutputStream.READ=" + out);

            String cp = "none";
            try (InputStream src = new ByteArrayInputStream(new byte[] { 1, 2, 3 })) {
                Files.copy(src, f, LinkOption.NOFOLLOW_LINKS);
            } catch (UnsupportedOperationException e) {
                cp = "UnsupportedOperationException";
            } catch (Exception e) {
                cp = e.getClass().getSimpleName();
            }
            System.out.println("CK RNioNoFollow copyStream.NOFOLLOW=" + cp);

            // Anti-vacuity: the LEGAL spellings must still work, or a VM that
            // refused every option would satisfy all three refusals above.
            try (InputStream ok = Files.newInputStream(f, StandardOpenOption.READ)) {
                System.out.println("CK RNioNoFollow newInputStream.READ=ok");
            }
            try (OutputStream ok = Files.newOutputStream(f, StandardOpenOption.WRITE)) {
                System.out.println("CK RNioNoFollow newOutputStream.WRITE=ok");
            }
            try (InputStream src = new ByteArrayInputStream(new byte[] { 1, 2, 3 })) {
                Files.copy(src, f, StandardCopyOption.REPLACE_EXISTING);
                System.out.println("CK RNioNoFollow copyStream.REPLACE=ok");
            }
        } finally {
            Files.deleteIfExists(f);
        }
    }

    /**
     * Every arm whose subject is a symbolic link. Runs only where the platform
     * lets an unprivileged process create one; the count on the PASS line is
     * what makes its absence visible rather than silent.
     */
    static String symlinkArms() throws Exception {
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
        // These are symlink controls: without them a blanket "always throw on a
        // symlink" regression would satisfy every refusal above.
        Path follow = link("h", true);
        Files.writeString(follow, "written", StandardOpenOption.TRUNCATE_EXISTING,
                StandardOpenOption.CREATE);
        check(content(target("h")).equals("written"), "without NOFOLLOW_LINKS the write follows");
        check(Files.isSymbolicLink(follow), "the link itself must survive the write");

        // Only the FINAL component is the option's business: a symlinked
        // directory earlier in the path is followed as normal.
        Path realDir = Files.createDirectory(dir.resolve("realdir"));
        Path dirLink = dir.resolve("dirlink");
        Files.createSymbolicLink(dirLink, realDir);
        Files.writeString(dirLink.resolve("inner"), "inner", StandardOpenOption.CREATE,
                LinkOption.NOFOLLOW_LINKS);
        check(content(realDir.resolve("inner")).equals("inner"),
                "NOFOLLOW_LINKS must only inspect the final path component");

        return writeString + "," + dangling + "," + outStream + "," + inStream + ","
                + byteChannel + "," + fileChannel + "," + bufWriter;
    }

    /**
     * Every arm that needs no symbolic link at all. These travel the SAME
     * option scanner the defect lived in — it read APPEND and CREATE_NEW and
     * nothing else, which is why NOFOLLOW_LINKS went unseen — so they are the
     * part of this vector that is still meaningful on a host where an
     * unprivileged process cannot create a link. They used to be discarded
     * along with the symlink arms by a single over-broad bail-out.
     */
    static void plainFileArms() throws Exception {
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
        System.out.println("CK RNioNoFollow createNew=" + createNew);

        // ------------------------------------------------------------------
        // NOFOLLOW_LINKS ASSERTED WITHOUT A SYMBOLIC LINK.
        //
        // Everything above about NOFOLLOW_LINKS needs `symlinkArms()`, which
        // needs a privilege Windows does not grant — so on the primary platform
        // this vector has never once executed an assertion about that option
        // against a link. This block is the part of that gap that CAN be closed
        // unprivileged, and it closes it from the other side: not "what does the
        // option DO to a link", but "is the option list read at all".
        //
        // That is the same question the defect was. The scanner read APPEND and
        // CREATE_NEW and nothing else, so an option it did not recognise was
        // silently accepted and ignored — which is exactly how NOFOLLOW_LINKS
        // came to be honoured nowhere. JDK 25 REFUSES three option/method pairs
        // outright, with no file access and no link involved:
        //
        //   Files.copy(InputStream, Path, CopyOption...)   Files.java
        //       every option but REPLACE_EXISTING -> UnsupportedOperationException
        //   FileSystemProvider.newInputStream(Path, OpenOption...)
        //       APPEND or WRITE -> UnsupportedOperationException
        //   FileSystemProvider.newOutputStream(Path, OpenOption...)
        //       READ -> IllegalArgumentException
        //
        // A scanner that ignores what it does not recognise answers "accepted"
        // to all three.
        //
        // ONLY THE FIRST OF THE THREE IS ASSERTED HERE, and the reason is a
        // reachability fact rather than caution. `Files.copy(InputStream, Path,
        // CopyOption[])` has exactly ONE registration tree-wide
        // (`register_p71_files_bridge` in
        // native-builtins/src/phases_late/nio_file.rs), and that registrar is
        // reached only from `register_synthetic_overrides` — so in BOTH shipping
        // modes the real JDK bytecode above runs and the refusal is the JDK's
        // own. The other two are served by shipping natives
        // (`fsp_new_input_stream` / `fsp_new_output_stream`, registered by
        // `register_phase57_nio_file`) which scan the option list for
        // NOFOLLOW_LINKS and nothing else, so they accept `WRITE` on an input
        // stream and `READ` on an output stream where the JDK refuses both.
        // Those two rows are recorded in W7-8-fabricated-success-io-sweep.md
        // with the patch that unlocks them; adding them here before that patch
        // would only paint a known, unowned divergence red.
        Path optSrc = dir.resolve("optionScan.src");
        Files.writeString(optSrc, "payload");
        Path optDst = dir.resolve("optionScan.dst");
        String copyNofollow;
        try (InputStream in = Files.newInputStream(optSrc)) {
            Files.copy(in, optDst, LinkOption.NOFOLLOW_LINKS);
            copyNofollow = "accepted";
        } catch (UnsupportedOperationException expected) {
            copyNofollow = "uoe";
        } catch (Exception other) {
            copyNofollow = "wrong-type:" + other.getClass().getName();
        }
        check(copyNofollow.equals("uoe"),
                "Files.copy(stream, path, NOFOLLOW_LINKS) must be refused: " + copyNofollow);
        check(!Files.exists(optDst), "a refused copy must not have created the target");
        System.out.println("CK RNioNoFollow copyStreamNofollow=" + copyNofollow);

        // The positive controls, and these ARE hard assertions: without them a
        // VM that refused EVERY option list would satisfy all three rows above
        // and look like the fix. `Files.copy(InputStream, Path)` with no options
        // and with REPLACE_EXISTING is the pair the JDK does accept.
        Path plainCopy = dir.resolve("optionScan.plain");
        try (InputStream in = Files.newInputStream(optSrc)) {
            check(Files.copy(in, plainCopy) == 7, "Files.copy(stream, path) must copy 7 bytes");
        }
        check(content(plainCopy).equals("payload"), "copied content: " + content(plainCopy));
        try (InputStream in = Files.newInputStream(optSrc)) {
            Files.copy(in, plainCopy, java.nio.file.StandardCopyOption.REPLACE_EXISTING);
        }
        check(content(plainCopy).equals("payload"), "REPLACE_EXISTING must be accepted");
        // ... and an existing target WITHOUT that option is a refusal, so the
        // option is doing work rather than being ignored in the other direction.
        String noReplace = outcome(() -> {
            try (InputStream in = Files.newInputStream(optSrc)) {
                Files.copy(in, plainCopy);
            }
        });
        check(noReplace.equals("io"),
                "copy onto an existing target without REPLACE_EXISTING: " + noReplace);

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
        // The content read back, as a value the diff can see. A VM that
        // garbled, short-read or zero-filled any part of the 4 MiB round trip
        // moves this number; the assertion above is the local half and this is
        // the cross-VM half, and they fail independently.
        System.out.println("CK RNioNoFollow bigRoundTrip=" + readBack.length + ","
                + java.util.Arrays.hashCode(readBack));
        Files.write(big, new byte[] { 'a', 'b', 'c' });
        check(Files.size(big) == 3, "a shorter write must truncate, size=" + Files.size(big));
        System.out.println("CK RNioNoFollow truncatedSize=" + Files.size(big));

        // Files.write(Path, Iterable) terminates each element with
        // System.lineSeparator(), NOT with '\n'. The assertion here read
        // `equals("alpha\nbeta\n")`, which is false on Windows — and this line
        // had never once executed there, because the symlink bail-out above
        // returned before reaching it. It is the first thing scoping that
        // bail-out uncovered, and it is a defect in the VECTOR, not in any VM.
        //
        // The separator is published as bytes as well as consumed, so the
        // assertion cannot be satisfied by a VM whose Files.write and whose
        // System.lineSeparator() are wrong in the same direction: that VM
        // agrees with itself here but disagrees with the oracle on the CK line.
        String sep = System.lineSeparator();
        Path lines = dir.resolve("lines");
        Files.write(lines, java.util.List.of("alpha", "beta"));
        check(content(lines).equals("alpha" + sep + "beta" + sep),
                "Iterable write: " + content(lines).replace("\r", "\\r").replace("\n", "\\n"));
        StringBuilder sepBytes = new StringBuilder();
        for (byte b : sep.getBytes(StandardCharsets.UTF_8)) {
            sepBytes.append(String.format("%02x", b));
        }
        System.out.println("CK RNioNoFollow lineSep=" + sepBytes);

        // The separator must be a TERMINATOR, not part of the content: reading
        // the same file back as lines has to give the two elements exactly. A
        // VM that appended the separator to the last element's TEXT rather than
        // after it passes the byte comparison above and fails here.
        check(Files.readAllLines(lines).equals(java.util.List.of("alpha", "beta")),
                "the separator terminates lines rather than joining them: "
                        + Files.readAllLines(lines));

        // THE OTHER HALF OF THE SAME MISTAKE, and the reason it is asserted
        // rather than assumed. `Files.write(Path, Iterable)` appends a separator
        // after every element; `Files.writeString` and `Files.write(Path,
        // byte[])` append NOTHING. A repair that reaches for "line-oriented
        // output" as one category adds a separator where the JDK adds none, and
        // every assertion above still passes. These two are the guard against
        // fixing it backwards.
        Path exact = dir.resolve("exact");
        Files.writeString(exact, "no trailing separator");
        check(content(exact).equals("no trailing separator"),
                "Files.writeString must append nothing: "
                        + content(exact).replace("\r", "\\r").replace("\n", "\\n"));
        Path exactBytes = dir.resolve("exactBytes");
        Files.write(exactBytes, "raw".getBytes(StandardCharsets.UTF_8));
        check(Files.size(exactBytes) == 3,
                "Files.write(byte[]) must append nothing, size=" + Files.size(exactBytes));

        // `BufferedWriter.newLine()` is what the JDK's own Iterable overload
        // calls, so it is the same contract one layer down — and it carries
        // more than one registration in this VM, of which only the last one
        // wins. A losing copy that answers '\n' is invisible until a
        // registration-order change promotes it.
        Path bw = dir.resolve("bufferedNewLine");
        try (java.io.BufferedWriter w = Files.newBufferedWriter(bw)) {
            w.write("x");
            w.newLine();
            w.write("y");
            w.newLine();
        }
        check(content(bw).equals("x" + sep + "y" + sep),
                "BufferedWriter.newLine emits the platform separator: "
                        + content(bw).replace("\r", "\\r").replace("\n", "\\n"));

        // `%n` and `println` are SPECIFIED to use the platform separator too,
        // so if they are wrong they are the same defect with a far wider blast
        // radius. Asserted against System.lineSeparator() rather than against a
        // literal, so this file states one rule and checks it four ways.
        check(String.format("a%nb").equals("a" + sep + "b"),
                "Formatter %n is the platform separator: "
                        + String.format("a%nb").replace("\r", "\\r").replace("\n", "\\n"));
        java.io.ByteArrayOutputStream captured = new java.io.ByteArrayOutputStream();
        try (java.io.PrintStream ps = new java.io.PrintStream(captured, true, "UTF-8")) {
            ps.println("p");
        }
        String printed = captured.toString("UTF-8");
        check(printed.equals("p" + sep),
                "PrintStream.println ends with the platform separator: "
                        + printed.replace("\r", "\\r").replace("\n", "\\n"));
    }
}
