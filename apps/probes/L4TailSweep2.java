import java.io.*;
import java.nio.*;
import java.nio.charset.*;
import java.nio.file.*;
import java.nio.file.attribute.*;
import java.nio.file.spi.FileSystemProvider;
import java.util.*;

/** L4 — the rows the first six probes could not reach, and why they could not.
 *
 *  The census says 142 of this lane's 395 owned bridge-with-code rows were
 *  never invoked by ANY probe. They are not one population:
 *
 *  1. COVARIANT BRIDGE DESCRIPTORS. `ByteBuffer.clear()` is declared to return
 *     `ByteBuffer`, and javac emits exactly that descriptor — so the inherited
 *     `clear()Ljava/nio/Buffer;` bridge is registered, owns its slot, and is
 *     never called from ordinary source. MEASURED on the earlier sweep:
 *
 *         clear ()Ljava/nio/ByteBuffer;   17 invocations
 *         clear ()Ljava/nio/Buffer;        0
 *         limit (I)Ljava/nio/ByteBuffer;  27
 *         limit (I)Ljava/nio/Buffer;       0
 *
 *     The bridge is reachable — you have to type the REFERENCE as `Buffer`,
 *     which is what any library taking a `Buffer` parameter does. The question
 *     worth asking is whether the bridge and its target agree, including on
 *     the IDENTITY of the returned object, because a bridge that copies is
 *     indistinguishable from one that does not until someone chains a call.
 *
 *  2. Overloads the earlier probes only exercised in their default-argument
 *     form: the CHARSET variants of `Files.readString`/`readAllLines`, the
 *     `Iterable` variants of `Files.write`, `BAOS.toString(Charset)`, the
 *     two-argument `File.setReadable(boolean, boolean)`.
 *
 *  3. The `FileSystemProvider` surface behind the link operations.
 *
 *  Hygiene as everywhere in this lane: no addresses, no identity hashes, and
 *  every directory listing sorted.
 */
public class L4TailSweep2 {
    static int rows = 0;
    static final String CWD = cwd();
    static String cwd() {
        String s = new File(".").getAbsolutePath();
        if (s.endsWith(File.separator + ".")) s = s.substring(0, s.length() - 2);
        return s;
    }
    static String esc(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '\n') b.append("\\n");
            else if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v).replace(CWD, "<CWD>")) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        rows++;
        try { r.run(); System.out.println(esc(tag) + " |no-throw|"); }
        catch (Throwable e) { System.out.println(esc(tag) + " |THREW " + e.getClass().getName() + "|"); }
    }
    interface ThrowingRun { void run() throws Throwable; }
    static String st(Buffer b) {
        return "p=" + b.position() + " l=" + b.limit() + " c=" + b.capacity();
    }
    static String hex(byte[] a) {
        if (a == null) return "null";
        StringBuilder s = new StringBuilder();
        for (byte x : a) s.append(String.format("%02x", x));
        return s.toString();
    }

    // ------------------------------------------------- 1. the bridge methods

    /** Everything here is called through a `Buffer`-typed reference ON PURPOSE.
     *  That is what makes javac emit the `()Ljava/nio/Buffer;` descriptors,
     *  which no other probe in the tree reaches. */
    static void bridges(String k, Buffer b) {
        p(k + " fresh", st(b));
        // Each of these returns `Buffer` at THIS call site. The identity check
        // is the point: the bridge must hand back the receiver, not a copy.
        Buffer r1 = b.position(1);
        p(k + " position(1) returns this", r1 == b);
        p(k + " after position(1)", st(b));
        Buffer r2 = b.limit(3);
        p(k + " limit(3) returns this", r2 == b);
        p(k + " after limit(3)", st(b));
        Buffer r3 = b.mark();
        p(k + " mark() returns this", r3 == b);
        b.position(2);
        Buffer r4 = b.reset();
        p(k + " reset() returns this", r4 == b);
        p(k + " after reset", st(b));
        Buffer r5 = b.rewind();
        p(k + " rewind() returns this", r5 == b);
        p(k + " after rewind", st(b));
        Buffer r6 = b.flip();
        p(k + " flip() returns this", r6 == b);
        p(k + " after flip", st(b));
        Buffer r7 = b.clear();
        p(k + " clear() returns this", r7 == b);
        p(k + " after clear", st(b));
        // The base-class accessors, reached through the same reference.
        p(k + " capacity", b.capacity());
        p(k + " limit", b.limit());
        p(k + " position", b.position());
        p(k + " remaining", b.remaining());
        p(k + " hasRemaining", b.hasRemaining());
        p(k + " hasArray", b.hasArray());
        p(k + " isDirect", b.isDirect());
        p(k + " isReadOnly", b.isReadOnly());
        // And the refusals, through the bridge descriptor.
        t(k + " position(-1)", () -> b.position(-1));
        t(k + " limit(cap+1)", () -> b.limit(b.capacity() + 1));
        b.clear();
        t(k + " reset with no mark", () -> b.reset());
        p(k + " toString", b.toString());
    }

    static void directAbsolute() {
        // The absolute float/double accessors on a DIRECT buffer -- the four
        // rows no probe reached, because the earlier sweep asked absolute
        // getInt/getLong/getShort/getChar and stopped there.
        ByteBuffer d = ByteBuffer.allocateDirect(32);
        d.putDouble(0, 2.5);
        d.putFloat(8, 1.5f);
        p("direct absolute putDouble leaves position", st(d));
        p("direct absolute getDouble(0)", d.getDouble(0));
        p("direct absolute getFloat(8)", d.getFloat(8));
        d.order(ByteOrder.LITTLE_ENDIAN);
        d.putDouble(16, 2.5);
        d.putFloat(24, 1.5f);
        p("direct LE getDouble(16)", d.getDouble(16));
        p("direct LE getFloat(24)", d.getFloat(24));
        t("direct getDouble(-1)", () -> d.getDouble(-1));
        t("direct getDouble(cap-7)", () -> d.getDouble(d.capacity() - 7));
        t("direct putFloat(cap-3)", () -> d.putFloat(d.capacity() - 3, 1f));
        ByteBuffer ro = d.asReadOnlyBuffer();
        t("direct readOnly putDouble", () -> ro.putDouble(0, 1.0));
        t("direct readOnly putFloat", () -> ro.putFloat(0, 1f));
        p("direct readOnly getDouble(0)", ro.getDouble(0));
    }

    // -------------------------------------------------- 2. the tail overloads

    static void baos() throws Exception {
        t("new BAOS(-1)", () -> new ByteArrayOutputStream(-1));
        ByteArrayOutputStream z = new ByteArrayOutputStream(0);
        z.write(7);
        p("BAOS(0) grows", hex(z.toByteArray()));
        ByteArrayOutputStream b = new ByteArrayOutputStream(4);
        b.write("héllo".getBytes(StandardCharsets.UTF_8));
        p("toString()", b.toString());
        p("toString(\"UTF-8\")", b.toString("UTF-8"));
        p("toString(UTF_8)", b.toString(StandardCharsets.UTF_8));
        p("toString(ISO_8859_1)", b.toString(StandardCharsets.ISO_8859_1));
        t("toString(\"no-such-charset\")", () -> b.toString("no-such-charset"));
        t("toString((String) null)", () -> b.toString((String) null));
        t("toString((Charset) null)", () -> b.toString((Charset) null));
        // The deprecated int-hibyte form, which is a DIFFERENT method.
        p("toString(0) hibyte", b.toString(0).length());
    }

    static void filePerms() throws Exception {
        File f = new File("l4tail2-perm.txt");
        try {
            try (FileOutputStream o = new FileOutputStream(f)) { o.write(1); }
            // The two-argument forms: ownerOnly true and false.
            p("setReadable(true,true)", f.setReadable(true, true));
            p("setReadable(false,true)", f.setReadable(false, true));
            p("canRead after setReadable(false,true)", f.canRead());
            p("setReadable(true,false)", f.setReadable(true, false));
            p("setExecutable(true,true)", f.setExecutable(true, true));
            p("canExecute after", f.canExecute());
            p("setExecutable(false,false)", f.setExecutable(false, false));
            p("canExecute after clear", f.canExecute());
            p("setReadable(true,true) on missing", new File("l4tail2-nope").setReadable(true, true));
            p("setExecutable(true,true) on missing", new File("l4tail2-nope").setExecutable(true, true));
        } finally { f.delete(); }
    }

    static void filterOutputStream() throws Exception {
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        FilterOutputStream f = new FilterOutputStream(sink);
        f.write(65);
        f.write(new byte[]{66, 67});
        f.write(new byte[]{68, 69, 70}, 1, 2);
        f.flush();
        p("FilterOutputStream wrote", sink.toString());
        t("write(null)", () -> f.write((byte[]) null));
        t("write(null,0,1)", () -> f.write(null, 0, 1));
        t("write(b,-1,1)", () -> f.write(new byte[2], -1, 1));
        t("write(b,0,9)", () -> f.write(new byte[2], 0, 9));
        f.close();
        t("close twice", () -> f.close());
        t("write after close", () -> f.write(1));
        // A FilterOutputStream over a null sink: the constructor accepts it.
        t("new FilterOutputStream(null)", () -> new FilterOutputStream(null));
        t("write through a null sink", () -> new FilterOutputStream(null).write(1));
    }

    static void printWriterTail() throws Exception {
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        PrintWriter w = new PrintWriter(sink);
        w.println();
        w.println((Object) "obj");
        w.println((Object) null);
        w.print("x");
        w.flush();
        p("PrintWriter(OutputStream) output", sink.toString());
        p("PrintWriter checkError", w.checkError());
        w.close();
        StringWriter sw = new StringWriter();
        PrintWriter w2 = new PrintWriter(sw);
        w2.println((Object) new Object() { public String toString() { return "T"; } });
        w2.flush();
        p("println(Object) uses toString", sw.toString());
    }

    // ------------------------------------------- 3. Files/provider tail

    static Path BASE;
    static void rmrf(Path p2) {
        try {
            if (Files.isDirectory(p2, LinkOption.NOFOLLOW_LINKS)) {
                try (DirectoryStream<Path> s = Files.newDirectoryStream(p2)) {
                    for (Path k : s) rmrf(k);
                }
            }
            Files.deleteIfExists(p2);
        } catch (IOException e) { }
    }

    static void filesTail() throws Exception {
        BASE = Paths.get("l4tail2");
        rmrf(BASE);
        Files.createDirectories(BASE);
        Path f = BASE.resolve("f.txt");
        Files.write(f, "one\ntwo\n".getBytes(StandardCharsets.UTF_8));

        // The CHARSET overloads, which the earlier probe only asked in their
        // default-charset form.
        p("readString(p, UTF_8)", Files.readString(f, StandardCharsets.UTF_8));
        p("readAllLines(p, UTF_8)", Files.readAllLines(f, StandardCharsets.UTF_8));
        p("readString(p, ISO_8859_1)", Files.readString(f, StandardCharsets.ISO_8859_1));
        Path bad = BASE.resolve("bad.bin");
        Files.write(bad, new byte[]{(byte) 0xC3, (byte) 0x28});
        t("readString(bad, UTF_8)", () -> Files.readString(bad, StandardCharsets.UTF_8));
        p("readString(bad, ISO_8859_1) len", Files.readString(bad, StandardCharsets.ISO_8859_1).length());
        t("readString(p, (Charset) null)", () -> Files.readString(f, (Charset) null));
        // `readAllLines` shares ONE body with its no-charset twin, so it needs
        // the same non-UTF-8 row -- a latin-1 decode cannot fail, and asking
        // only the UTF-8 overload cannot tell an honoured charset from an
        // ignored one.
        p("readAllLines(bad, ISO_8859_1) size", Files.readAllLines(bad, StandardCharsets.ISO_8859_1).size());
        p("readAllLines(bad, ISO_8859_1) len", Files.readAllLines(bad, StandardCharsets.ISO_8859_1).get(0).length());
        t("readAllLines(bad, UTF_8)", () -> Files.readAllLines(bad, StandardCharsets.UTF_8));
        t("readAllLines(p, (Charset) null)", () -> Files.readAllLines(f, (Charset) null));
        p("readString(p, US_ASCII)", Files.readString(f, StandardCharsets.US_ASCII));
        // US-ASCII REPORTS rather than replaces: `Files.readString` decodes
        // with a CharsetDecoder left on its default REPORT action, so a byte
        // above 0x7F is a refusal and not a U+FFFD. Latin-1 two lines up
        // cannot fail at all. The three charsets answer three different ways
        // to the same two bytes, which is what makes them worth asking.
        t("readString(bad, US_ASCII)", () -> Files.readString(bad, StandardCharsets.US_ASCII));

        // The Iterable write overloads.
        Path it = BASE.resolve("it.txt");
        p("write(p, Iterable)", Files.write(it, Arrays.asList("a", "b")).getFileName());
        p("write(p, Iterable) content", Files.readString(it).replace("\n", "|"));
        p("write(p, Iterable, cs)", Files.write(it, Arrays.asList("c"), StandardCharsets.UTF_8).getFileName());
        p("write(p, Iterable, cs) truncates", Files.readString(it).replace("\n", "|"));
        p("write(p, Iterable, APPEND)", Files.write(it, Arrays.asList("d"), StandardOpenOption.APPEND).getFileName());
        p("after append", Files.readString(it).replace("\n", "|"));
        t("write(p, (Iterable) null)", () -> Files.write(it, (Iterable<CharSequence>) null));
        p("write(p, empty Iterable)", Files.write(BASE.resolve("e.txt"), new ArrayList<String>()).getFileName());
        p("empty Iterable wrote nothing", Files.size(BASE.resolve("e.txt")));

        // The 4-argument walkFileTree, and its depth argument.
        Path deep = BASE.resolve("d1/d2");
        Files.createDirectories(deep);
        Files.write(deep.resolve("leaf.txt"), new byte[]{1});
        List<String> seen = new ArrayList<>();
        Files.walkFileTree(BASE.resolve("d1"), EnumSet.noneOf(FileVisitOption.class), 1,
            new SimpleFileVisitor<Path>() {
                public FileVisitResult preVisitDirectory(Path p2, BasicFileAttributes a) {
                    seen.add("pre:" + BASE.relativize(p2)); return FileVisitResult.CONTINUE;
                }
                public FileVisitResult visitFile(Path p2, BasicFileAttributes a) {
                    seen.add("file:" + BASE.relativize(p2)); return FileVisitResult.CONTINUE;
                }
            });
        Collections.sort(seen);
        p("walkFileTree depth 1", seen);
        t("walkFileTree depth -1", () -> Files.walkFileTree(BASE, EnumSet.noneOf(FileVisitOption.class), -1,
            new SimpleFileVisitor<Path>() {}));
        t("walkFileTree null options", () -> Files.walkFileTree(BASE, null, 1, new SimpleFileVisitor<Path>() {}));

        // readSymbolicLink / createSymbolicLink / createLink, which reach the
        // provider directly. All wrapped: a host may refuse links entirely,
        // and the two VMs must refuse the same way.
        Path link = BASE.resolve("link");
        t("createSymbolicLink", () -> Files.createSymbolicLink(link, f));
        t("readSymbolicLink(link)", () -> Files.readSymbolicLink(link));
        p("isSymbolicLink", Files.isSymbolicLink(link));
        t("readSymbolicLink(regular file)", () -> Files.readSymbolicLink(f));
        t("readSymbolicLink(missing)", () -> Files.readSymbolicLink(BASE.resolve("nope")));
        Path hard = BASE.resolve("hard");
        t("createLink", () -> Files.createLink(hard, f));
        t("createLink onto existing", () -> Files.createLink(hard, f));
        t("createLink from missing", () -> Files.createLink(BASE.resolve("h2"), BASE.resolve("nope")));

        // getOwner: the NAME is host-dependent, so only its shape is asserted.
        t("getOwner non-null", () -> {
            UserPrincipal u = Files.getOwner(f);
            if (u == null || u.getName().isEmpty()) throw new IllegalStateException("empty");
        });
        t("getOwner(missing)", () -> Files.getOwner(BASE.resolve("nope")));

        // The provider surface itself.
        FileSystemProvider prov = FileSystems.getDefault().provider();
        p("provider scheme", prov.getScheme());
        p("installedProviders non-empty", !FileSystemProvider.installedProviders().isEmpty());
        p("installedProviders contains default",
          FileSystemProvider.installedProviders().stream().anyMatch(q -> "file".equals(q.getScheme())));
        t("provider.newInputStream", () -> prov.newInputStream(f).close());
        t("provider.newInputStream(missing)", () -> prov.newInputStream(BASE.resolve("nope")).close());
        t("newFileSystem(Path, Map) on a text file", () -> FileSystems.newFileSystem(f, (Map<String, ?>) null));
        rmrf(BASE);
    }

    public static void main(String[] a) throws Exception {
        bridges("heap", ByteBuffer.allocate(8));
        bridges("direct", ByteBuffer.allocateDirect(8));
        bridges("intview", ByteBuffer.allocate(32).asIntBuffer());
        bridges("charbuf", CharBuffer.allocate(8));
        directAbsolute();
        baos();
        filePerms();
        filterOutputStream();
        printWriterTail();
        filesTail();
        System.out.println("rows " + rows);
        System.out.println("DONE L4TailSweep2");
    }
}
