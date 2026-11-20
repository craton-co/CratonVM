import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * `Scanner(InputStream)` over stream shapes other than the two this VM
 * duck-types for.
 *
 * `native_scanner_init_inputstream` decides what its source is by LOOKING AT
 * THE RECEIVER'S SLOTS: `(Object, Int, Int)` in 0/1/2 means "a
 * `ByteArrayInputStream`, read its `buf`", and `Int` in slot 0 means "a
 * synthetic `FileInputStream`, read that fd". A stream that is neither — and
 * `java.io.BufferedInputStream` is the commonest one in the language — matches
 * no branch.
 *
 * That duck test has already been wrong once in this file: `Scanner(Readable)`
 * used to be pointed at this same body, and a `StringReader`'s
 * `{str, length, next, mark}` layout MATCHED the byte-array shape test, so it
 * read array elements out of a `String` and produced an empty scanner rather
 * than an error.
 *
 * Every case below prints what the scanner actually yielded. An empty scanner
 * where HotSpot yields tokens is a silent wrong answer, not an exception.
 */
public class W4Scanner {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    static void drain(String tag, Scanner sc) {
        try {
            List<String> tokens = new ArrayList<>();
            while (sc.hasNext() && tokens.size() < 10) tokens.add(sc.next());
            ck(tag, tokens);
        } catch (Throwable t) {
            ck(tag, "threw:" + t.getClass().getName());
        }
    }

    public static void main(String[] args) throws Exception {
        byte[] payload = "alpha beta 42 gamma".getBytes(StandardCharsets.UTF_8);

        // The two shapes the duck test knows.
        drain("scan.bais", new Scanner(new ByteArrayInputStream(payload)));
        drain("scan.string", new Scanner("alpha beta 42 gamma"));

        // The one every wrapper produces.
        drain("scan.buffered.bais",
                new Scanner(new BufferedInputStream(new ByteArrayInputStream(payload))));
        drain("scan.buffered.bais.charset",
                new Scanner(new BufferedInputStream(new ByteArrayInputStream(payload)),
                        StandardCharsets.UTF_8));

        // A file, direct and wrapped.
        Path dir = Files.createTempDirectory("w4scan");
        Path f = dir.resolve("in.txt");
        Files.write(f, payload);
        try (InputStream in = new FileInputStream(f.toFile())) {
            drain("scan.fis", new Scanner(in));
        }
        try (InputStream in = new BufferedInputStream(new FileInputStream(f.toFile()))) {
            drain("scan.buffered.fis", new Scanner(in));
        }
        try (InputStream in = Files.newInputStream(f)) {
            drain("scan.files.newInputStream", new Scanner(in));
        }

        // A DataInputStream and a PushbackInputStream, two more FilterInputStreams.
        try (InputStream in = new DataInputStream(new ByteArrayInputStream(payload))) {
            drain("scan.dis", new Scanner(in));
        }
        try (InputStream in = new PushbackInputStream(new ByteArrayInputStream(payload))) {
            drain("scan.pushback", new Scanner(in));
        }

        // A user subclass of InputStream: only the abstract primitive.
        drain("scan.userStream", new Scanner(new InputStream() {
            private int i;
            @Override public int read() { return i < payload.length ? (payload[i++] & 0xff) : -1; }
        }));

        // A SequenceInputStream, which is what a concatenation produces.
        drain("scan.sequence", new Scanner(new SequenceInputStream(
                new ByteArrayInputStream("alpha beta ".getBytes(StandardCharsets.UTF_8)),
                new ByteArrayInputStream("42 gamma".getBytes(StandardCharsets.UTF_8)))));

        // Typed reads through the wrapped shape, since `next()` and `nextInt()`
        // take different paths inside the scanner.
        Scanner typed = new Scanner(new BufferedInputStream(new ByteArrayInputStream(
                "7 8.5 true tail".getBytes(StandardCharsets.UTF_8))));
        try {
            ck("scan.buffered.nextInt", typed.nextInt());
            ck("scan.buffered.nextDouble", typed.nextDouble());
            ck("scan.buffered.nextBoolean", typed.nextBoolean());
            ck("scan.buffered.next", typed.next());
        } catch (Throwable t) {
            ck("scan.buffered.typed", "threw:" + t.getClass().getName());
        }

        // nextLine() over a wrapped multi-line stream.
        Scanner lines = new Scanner(new BufferedInputStream(new ByteArrayInputStream(
                "first line\nsecond line\n".getBytes(StandardCharsets.UTF_8))));
        try {
            ck("scan.buffered.line1", lines.nextLine());
            ck("scan.buffered.line2", lines.nextLine());
        } catch (Throwable t) {
            ck("scan.buffered.lines", "threw:" + t.getClass().getName());
        }

        // ---- the other constructor overloads --------------------------
        // Each is a separate registration in this VM, and a missing one does
        // not fail loudly: the real JDK constructor runs, sets the real
        // Scanner's fields, and this VM's `hasNext`/`next` natives then read
        // their OWN unset state and report an empty scanner.
        drain("scan.file", new Scanner(f.toFile()));
        drain("scan.file.charset", new Scanner(f.toFile(), StandardCharsets.UTF_8));
        drain("scan.file.charsetName", new Scanner(f.toFile(), "UTF-8"));
        drain("scan.path", new Scanner(f));
        drain("scan.path.charset", new Scanner(f, StandardCharsets.UTF_8));
        drain("scan.path.charsetName", new Scanner(f, "UTF-8"));
        drain("scan.stream.charsetName",
                new Scanner(new ByteArrayInputStream(payload), "UTF-8"));
        drain("scan.bais.charset",
                new Scanner(new ByteArrayInputStream(payload), StandardCharsets.UTF_8));
        try (java.nio.channels.ReadableByteChannel ch =
                     java.nio.channels.Channels.newChannel(new ByteArrayInputStream(payload))) {
            drain("scan.channel", new Scanner(ch));
        }
        drain("scan.readable", new Scanner(new StringReader("alpha beta 42 gamma")));
        // A non-UTF-8 charset, so a decoder that ignores its argument shows up.
        byte[] latin = "café naïve".getBytes(StandardCharsets.ISO_8859_1);
        drain("scan.latin1", new Scanner(new ByteArrayInputStream(latin),
                StandardCharsets.ISO_8859_1));
        drain("scan.latin1.asUtf8", new Scanner(new ByteArrayInputStream(latin),
                StandardCharsets.UTF_8));

        System.out.println("PASS W4Scanner");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
