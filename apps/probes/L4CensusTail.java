import java.io.*;
import java.net.URI;
import java.nio.*;
import java.nio.channels.FileChannel;
import java.nio.file.*;
import java.nio.file.attribute.*;
import java.nio.file.spi.FileSystemProvider;
import java.time.Instant;
import java.util.*;

/** L4 — the census tail: rows still never reached after parts one to four.
 *
 *  The completeness census, re-taken across all thirteen probes, says 157 of
 *  the lane's 454 adjudication rows have never been dispatched to. They are not
 *  one population, and most of the tail is NOT this lane's:
 *
 *    channels/selectors (~35)  network-shaped and unclaimed (record §P2.5)
 *    Buffer$2 / FileDescriptor$1 (22)  SharedSecrets access bridges
 *    java.io exception classes (~13)   the shared Throwable table
 *    UnixFileSystem (12)              driven indirectly through java.io.File
 *
 *  What IS this lane's, and drivable from ordinary Java, is here. Three groups
 *  worth naming:
 *
 *  1. TWO DEPRECATED STREAMS NOBODY PROBES. `LineNumberInputStream` (8 rows)
 *     and `StringBufferInputStream` (5) are deprecated, still shipped, still
 *     registered, and never once exercised. `LineNumberInputStream` in
 *     particular collapses CR, LF and CRLF to a single '\n' AND counts lines
 *     while doing it, so its read/mark/reset interact in a way nothing else in
 *     java.io does.
 *
 *  2. TYPE-ERASURE BRIDGES. `SimpleFileVisitor`'s four
 *     `(Ljava/lang/Object;...)` rows are the erased `FileVisitor` bridges.
 *     Calling them needs a reference typed as the INTERFACE, not as
 *     `SimpleFileVisitor<Path>` -- the same shape part four found in
 *     `reset()Ljava/nio/Buffer;`, arriving from generics instead of covariant
 *     returns. Two different language features, one dispatch consequence.
 *
 *  3. THE VIEW-BUFFER toString(II) FAMILY. `ByteBufferAsCharBufferB/L/RB/RL`
 *     each register their own `toString(II)`; reaching all four needs
 *     big-endian, little-endian, and the read-only variant of each.
 *
 *  Rows deliberately NOT probed, because they are unreachable BY CONSTRUCTION
 *  rather than merely unprobed -- see the record's §P5.2: registrations on
 *  `java/nio/Buffer`, `java/io/OutputStream` and `java/io/FilterInputStream`
 *  key on a receiver class that no live object ever has.
 */
public class L4CensusTail {
    static int rows = 0;

    interface ThrowingRun { void run() throws Throwable; }
    interface ThrowingGet { Object get() throws Throwable; }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(tag + " |" + v + "|");
    }

    /** A value row that survives a refusal — see L4BridgeSweep's note. */
    static void pt(String tag, ThrowingGet g) {
        rows++;
        try { System.out.println(tag + " |" + g.get() + "|"); }
        catch (Throwable e) { System.out.println(tag + " |THREW " + e.getClass().getName() + "|"); }
    }

    static void t(String tag, ThrowingRun r) {
        rows++;
        try { r.run(); System.out.println(tag + " |no-throw|"); }
        catch (Throwable e) { System.out.println(tag + " |THREW " + e.getClass().getName() + "|"); }
    }

    // ============================================ 1. the two deprecated streams
    @SuppressWarnings("deprecation")
    static void lineNumberInputStream() throws Exception {
        // Every line terminator shape in one buffer: LF, CRLF, and a bare CR.
        // `LineNumberInputStream` maps all three to a single '\n' on read AND
        // increments its counter, so the byte it returns and the number it
        // reports are two answers that have to agree.
        byte[] raw = "a\nb\r\nc\rd".getBytes("US-ASCII");
        LineNumberInputStream in = new LineNumberInputStream(new ByteArrayInputStream(raw));
        p("LNIS initial line", in.getLineNumber());
        p("LNIS available", in.available());
        StringBuilder seen = new StringBuilder();
        List<Integer> lines = new ArrayList<>();
        int c;
        while ((c = in.read()) != -1) {
            seen.append((char) c);
            lines.add(in.getLineNumber());
        }
        // The bytes AFTER translation, rendered so a CR would be visible.
        p("LNIS bytes", seen.toString().replace("\n", "<NL>").replace("\r", "<CR>"));
        p("LNIS line after each byte", lines);
        p("LNIS final line", in.getLineNumber());
        p("LNIS read at EOF", in.read());
        p("LNIS available at EOF", in.available());
        in.close();

        // setLineNumber is arbitrary: the JDK does not validate it.
        LineNumberInputStream s = new LineNumberInputStream(new ByteArrayInputStream(raw));
        s.setLineNumber(41);
        p("LNIS after setLineNumber(41)", s.getLineNumber());
        s.read();
        s.read();
        p("LNIS line after two reads", s.getLineNumber());
        s.setLineNumber(-5);
        p("LNIS negative line accepted", s.getLineNumber());
        s.close();

        // mark/reset must restore the LINE NUMBER as well as the position —
        // the counter is part of the stream's state, and a reset that only
        // rewinds the bytes silently double-counts every line after it.
        LineNumberInputStream m = new LineNumberInputStream(new ByteArrayInputStream(raw));
        p("LNIS markSupported", m.markSupported());
        m.read();
        m.mark(32);
        int atMark = m.getLineNumber();
        p("LNIS line at mark", atMark);
        m.read(); m.read(); m.read();
        p("LNIS line after 3 more", m.getLineNumber());
        t("LNIS reset", () -> m.reset());
        p("LNIS line after reset", m.getLineNumber());
        p("LNIS byte after reset", (char) m.read());
        m.close();

        // The array read goes through the same translation.
        LineNumberInputStream b = new LineNumberInputStream(new ByteArrayInputStream(raw));
        byte[] buf = new byte[16];
        int n = b.read(buf, 0, buf.length);
        p("LNIS read([BII) n", n);
        p("LNIS read([BII) bytes",
          new String(buf, 0, Math.max(n, 0), "US-ASCII").replace("\n", "<NL>").replace("\r", "<CR>"));
        p("LNIS read([BII) line", b.getLineNumber());
        pt("LNIS read([BII) zero len", () -> b.read(buf, 0, 0));
        t("LNIS read([BII) null", () -> b.read(null, 0, 1));
        t("LNIS read([BII) bad off", () -> b.read(buf, -1, 1));
        b.close();

        p("LNIS skip", new LineNumberInputStream(new ByteArrayInputStream(raw)).skip(3));
    }

    @SuppressWarnings("deprecation")
    static void stringBufferInputStream() throws Exception {
        // The one that famously truncates: it keeps the LOW BYTE of each char,
        // so a non-Latin-1 string comes back as garbage rather than as an
        // encoding error. That is the documented reason it is deprecated, and
        // it is exactly the kind of thing a shadow gets wrong by "fixing".
        StringBufferInputStream in = new StringBufferInputStream("abé中");
        p("SBIS available", in.available());
        List<Integer> got = new ArrayList<>();
        int c;
        while ((c = in.read()) != -1) got.add(c);
        p("SBIS bytes", got);
        p("SBIS read at EOF", in.read());
        p("SBIS available at EOF", in.available());
        in.reset();
        p("SBIS available after reset", in.available());
        p("SBIS first byte after reset", in.read());
        in.close();

        StringBufferInputStream b = new StringBufferInputStream("hello");
        byte[] buf = new byte[8];
        p("SBIS read([BII) n", b.read(buf, 0, 8));
        p("SBIS read([BII) text", new String(buf, 0, 5, "US-ASCII"));
        p("SBIS read([BII) at EOF", b.read(buf, 0, 8));
        t("SBIS read([BII) null", () -> b.read(null, 0, 1));
        p("SBIS skip", new StringBufferInputStream("hello").skip(2));
        p("SBIS skip past end", new StringBufferInputStream("hello").skip(99));
        b.close();

        StringBufferInputStream e = new StringBufferInputStream("");
        p("SBIS empty available", e.available());
        p("SBIS empty read", e.read());
        e.close();
    }

    // ================================================ 2. the java.io stream odds
    static void streamOdds() throws Exception {
        DataInputStream d = new DataInputStream(
            new ByteArrayInputStream(new byte[] {(byte) 0xFF, (byte) 0xFE, 0x00, 0x01}));
        p("readUnsignedShort 1", d.readUnsignedShort());
        p("readUnsignedShort 2", d.readUnsignedShort());
        t("readUnsignedShort at EOF", () -> d.readUnsignedShort());
        d.close();

        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        DataOutputStream o = new DataOutputStream(sink);
        o.write(65);
        o.write(new byte[] {66, 67, 68}, 1, 2);
        o.flush();
        p("DOS wrote", sink.toString("US-ASCII"));
        p("DOS size", o.size());
        t("DOS write([BII) null", () -> o.write(null, 0, 1));
        t("DOS write([BII) bad range", () -> o.write(new byte[2], 1, 5));
        o.close();
    }

    // ============================================== 3. the buffer census tail
    static void bufferTail() {
        ByteBuffer bb = ByteBuffer.allocate(16);
        bb.put(new byte[] {1, 2, 3, 4});
        bb.flip();
        byte[] dst = new byte[4];
        bb.get(dst, 0, 4);
        p("ByteBuffer.get([BII)", Arrays.toString(dst));
        p("ByteBuffer.toString", ByteBuffer.allocate(4).toString());
        ByteBuffer p2 = ByteBuffer.allocate(8);
        p2.put(new byte[] {9, 8});
        p("ByteBuffer.put([B) pos", p2.position());
        t("ByteBuffer.get([BII) overflow", () -> ByteBuffer.allocate(2).get(new byte[4], 0, 4));
        t("ByteBuffer.put([B) overflow", () -> ByteBuffer.allocate(1).put(new byte[4]));

        // array() on every typed buffer, and its refusal on a read-only one.
        p("ShortBuffer.array len", ShortBuffer.allocate(3).array().length);
        p("IntBuffer.array len", IntBuffer.allocate(3).array().length);
        p("LongBuffer.array len", LongBuffer.allocate(3).array().length);
        p("FloatBuffer.array len", FloatBuffer.allocate(3).array().length);
        p("DoubleBuffer.array len", DoubleBuffer.allocate(3).array().length);
        t("IntBuffer.array on read-only", () -> IntBuffer.allocate(3).asReadOnlyBuffer().array());
        t("IntBuffer.array on a view", () -> ByteBuffer.allocate(16).asIntBuffer().array());

        // the bulk get([XII) family
        short[] sd = new short[2]; ShortBuffer.wrap(new short[] {1, 2}).get(sd, 0, 2);
        p("ShortBuffer.get([SII)", Arrays.toString(sd));
        long[] ld = new long[2]; LongBuffer.wrap(new long[] {1L, 2L}).get(ld, 0, 2);
        p("LongBuffer.get([JII)", Arrays.toString(ld));
        float[] fd = new float[2]; FloatBuffer.wrap(new float[] {1.5f, 2.5f}).get(fd, 0, 2);
        p("FloatBuffer.get([FII)", Arrays.toString(fd));
        double[] dd = new double[2]; DoubleBuffer.wrap(new double[] {1.5, 2.5}).get(dd, 0, 2);
        p("DoubleBuffer.get([DII)", Arrays.toString(dd));
        t("DoubleBuffer.get([DII) overflow",
          () -> DoubleBuffer.allocate(1).get(new double[4], 0, 4));

        // ByteOrder, and the four ByteBufferAsCharBuffer* toString(II) rows:
        // big/little endian x writable/read-only is what selects the class.
        p("ByteOrder.BIG toString", ByteOrder.BIG_ENDIAN.toString());
        p("ByteOrder.LITTLE toString", ByteOrder.LITTLE_ENDIAN.toString());
        p("nativeOrder is one of the two",
          ByteOrder.nativeOrder() == ByteOrder.BIG_ENDIAN
              || ByteOrder.nativeOrder() == ByteOrder.LITTLE_ENDIAN);

        for (ByteOrder order : new ByteOrder[] {ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN}) {
            String tag = order == ByteOrder.BIG_ENDIAN ? "BE" : "LE";
            ByteBuffer src = ByteBuffer.allocate(8).order(order);
            src.putChar('h').putChar('i').putChar('!').putChar('?');
            src.clear();
            CharBuffer view = src.asCharBuffer();
            p("view " + tag + " toString", view.toString());
            p("view " + tag + " order", view.order().toString());
            CharBuffer ro = view.asReadOnlyBuffer();
            p("view " + tag + " RO toString", ro.toString());
            p("view " + tag + " RO order", ro.order().toString());
            view.position(1); view.limit(3);
            p("view " + tag + " sliced toString", view.toString());
        }

        // the heap and string CharBuffer toString(II) rows
        CharBuffer hc = CharBuffer.allocate(6);
        hc.put("abcdef"); hc.flip();
        p("HeapCharBuffer toString", hc.toString());
        p("HeapCharBufferR toString", hc.asReadOnlyBuffer().toString());
        p("StringCharBuffer toString", CharBuffer.wrap("wrapped").toString());
        p("StringCharBuffer sliced", CharBuffer.wrap("wrapped").subSequence(2, 5).toString());
    }

    // ====================================== 4. MappedByteBuffer, on a real file
    static void mapped() throws Exception {
        Path f = Files.createTempFile("l4map", ".bin");
        Files.write(f, new byte[] {1, 2, 3, 4, 5, 6, 7, 8});
        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ,
                                               StandardOpenOption.WRITE)) {
            MappedByteBuffer m = ch.map(FileChannel.MapMode.READ_WRITE, 0, 8);
            p("mapped capacity", m.capacity());
            p("mapped first byte", m.get(0));
            p("mapped isDirect", m.isDirect());
            // `load`/`isLoaded`/`force` are advisory: the JDK is allowed to
            // answer either way for isLoaded, so only the TYPE of answer and
            // the identity of the return are comparable.
            pt("mapped load returns this", () -> m.load() == m);
            pt("mapped isLoaded is a boolean",
               () -> m.isLoaded() || !m.isLoaded());
            m.put(0, (byte) 99);
            pt("mapped force returns this", () -> m.force() == m);
            p("mapped byte after force", m.get(0));
        }
        p("file byte after force", Files.readAllBytes(f)[0]);
        Files.deleteIfExists(f);
    }

    // ================================= 5. the type-erasure bridges, and friends
    static void erasureBridgesAndPaths() throws Exception {
        Path dir = Files.createTempDirectory("l4tail");
        Path file = dir.resolve("f.txt");
        Files.writeString(file, "x");

        // THE ERASED BRIDGES. `SimpleFileVisitor<Path>` implements
        // `FileVisitor<Path>`; javac emits `visitFile(Object, BasicFileAttributes)`
        // bridges on the class. A reference typed `SimpleFileVisitor<Path>` calls
        // the specific method; a reference typed as the raw INTERFACE calls the
        // bridge. Same dispatch consequence as a covariant return, arriving from
        // a different language feature.
        SimpleFileVisitor<Path> v = new SimpleFileVisitor<Path>() {};
        @SuppressWarnings("rawtypes")
        FileVisitor raw = v;
        BasicFileAttributes attrs = Files.readAttributes(file, BasicFileAttributes.class);
        pt("bridge visitFile", () -> raw.visitFile(file, attrs));
        pt("bridge preVisitDirectory", () -> raw.preVisitDirectory(dir, attrs));
        pt("bridge postVisitDirectory null exc", () -> raw.postVisitDirectory(dir, null));
        t("bridge postVisitDirectory with exc",
          () -> raw.postVisitDirectory(dir, new IOException("boom")));
        t("bridge visitFileFailed", () -> raw.visitFileFailed(file, new IOException("boom")));
        // The specific methods, for the control: if these disagree with the
        // bridges above, the bridge and its target are two implementations.
        pt("direct visitFile", () -> v.visitFile(file, attrs));
        pt("direct preVisitDirectory", () -> v.preVisitDirectory(dir, attrs));
        pt("direct postVisitDirectory null exc", () -> v.postVisitDirectory(dir, null));

        p("FileVisitResult.values", Arrays.toString(FileVisitResult.values()));
        p("FileVisitResult.valueOf", FileVisitResult.valueOf("SKIP_SUBTREE"));
        t("FileVisitResult.valueOf bogus", () -> FileVisitResult.valueOf("NOPE"));
        t("FileVisitResult.valueOf null", () -> FileVisitResult.valueOf(null));

        p("PosixFilePermission.values", Arrays.toString(PosixFilePermission.values()));
        p("PosixFilePermission.valueOf", PosixFilePermission.valueOf("OWNER_WRITE"));
        t("PosixFilePermission.valueOf bogus", () -> PosixFilePermission.valueOf("NOPE"));

        // Path rows
        p("Path.of(URI)", Path.of(file.toUri()).getFileName().toString());
        t("Path.of(URI) not a file scheme", () -> Path.of(URI.create("http://example.com/x")));
        t("Path.of(URI) relative", () -> Path.of(URI.create("foo/bar")));
        p("resolveSibling(Path)", file.resolveSibling(Path.of("g.txt")).getFileName().toString());
        p("resolveSibling absolute",
          file.resolveSibling(Path.of("/tmp/abs")).toString());
        p("resolveSibling empty", file.resolveSibling(Path.of("")).toString().endsWith("/"));
        t("resolveSibling null", () -> file.resolveSibling((Path) null));

        try (WatchService ws = FileSystems.getDefault().newWatchService()) {
            pt("Path.register valid",
               () -> dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE) != null);
            t("Path.register on a FILE", () -> file.register(ws, StandardWatchEventKinds.ENTRY_CREATE));
            t("Path.register no kinds", () -> dir.register(ws));
            t("Path.register null service",
              () -> dir.register(null, StandardWatchEventKinds.ENTRY_CREATE));
        }

        // FileTime.from(Instant) — the Instant overload, not the millis one
        FileTime ft = FileTime.from(Instant.ofEpochMilli(1234567890000L));
        p("FileTime.from(Instant) millis", ft.toMillis());
        p("FileTime.from(Instant) roundtrip", ft.toInstant().toEpochMilli());
        p("FileTime.from(Instant) equals millis-built",
          ft.equals(FileTime.fromMillis(1234567890000L)));
        t("FileTime.from(null)", () -> FileTime.from((Instant) null));

        // The provider's own refusals, BY TYPE. `Files.createDirectories`
        // walks parents with `catch (NoSuchFileException)`, so a supertype
        // instance here breaks a caller three frames up rather than at the
        // throw -- assert the class, never just that something was thrown.
        FileSystemProvider prov = FileSystems.getDefault().provider();
        t("provider.checkAccess(missing)", () -> prov.checkAccess(dir.resolve("nope")));
        t("provider.checkAccess(present)", () -> prov.checkAccess(file));
        t("provider.readAttributes(missing)",
          () -> prov.readAttributes(dir.resolve("nope"), BasicFileAttributes.class));
        t("Files.readAttributes(missing)",
          () -> Files.readAttributes(dir.resolve("nope"), BasicFileAttributes.class));
        t("Files.createDirectory on an existing dir", () -> Files.createDirectory(dir));
        // The multi-level create is the caller that the type above breaks.
        Path deep = dir.resolve("d1/d2/d3");
        pt("createDirectories(3 levels)", () -> {
            Path r = Files.createDirectories(deep);
            return Files.isDirectory(r);
        });
        pt("createDirectories again is a no-op", () -> {
            Path r = Files.createDirectories(deep);
            return Files.isDirectory(r);
        });
        Files.deleteIfExists(deep);
        Files.deleteIfExists(deep.getParent());
        Files.deleteIfExists(deep.getParent().getParent());

        // FileStore.getBlockSize and the two ClassLoader newFileSystem overloads
        pt("FileStore.getBlockSize > 0", () -> Files.getFileStore(file).getBlockSize() > 0);
        t("newFileSystem(Path, ClassLoader) on a text file",
          () -> FileSystems.newFileSystem(file, (ClassLoader) null));
        t("newFileSystem(Path, Map, ClassLoader) null env",
          () -> FileSystems.newFileSystem(file, (Map<String, ?>) null, null));

        Files.deleteIfExists(file);
        Files.deleteIfExists(dir);
    }

    public static void main(String[] args) throws Exception {
        lineNumberInputStream();
        stringBufferInputStream();
        streamOdds();
        bufferTail();
        mapped();
        erasureBridgesAndPaths();
        System.out.println("rows " + rows);
        System.out.println("DONE L4CensusTail");
    }
}
