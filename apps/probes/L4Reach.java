import java.io.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.charset.*;
import java.nio.file.*;
import java.nio.file.attribute.*;
import java.util.*;

/** L4 REACH probe -- not a differential probe.
 *
 *  Its only job is to make a `--jdk-only-report` and a `--dump-native-registry`
 *  run over as much of `java.io` / `java.nio` as one process can touch, so the
 *  report's `native-shadows-bytecode` rows with `outcome=native-won` can be
 *  mined into the real L4 worklist. Correctness of any answer here is NOT the
 *  point; everything is wrapped so a throw never ends the run early (a crash
 *  partway through would silently truncate the worklist).
 *
 *  Never calls System.exit: the report is not written when it does.
 */
public class L4Reach {
    static int ok = 0, threw = 0;
    static void r(String tag, ThrowingRun t) {
        try { t.run(); ok++; } catch (Throwable e) { threw++; }
    }
    interface ThrowingRun { void run() throws Throwable; }

    static Path tmp;

    static void files() throws Exception {
        tmp = Files.createTempDirectory("l4reach");
        Path f = tmp.resolve("a.txt");
        Path g = tmp.resolve("b.txt");
        Path d = tmp.resolve("sub");
        r("w", () -> Files.write(f, new byte[]{1, 2, 3}));
        r("w2", () -> Files.writeString(g, "hello"));
        r("dir", () -> Files.createDirectory(d));
        r("dirs", () -> Files.createDirectories(tmp.resolve("x/y/z")));
        r("rb", () -> Files.readAllBytes(f));
        r("rl", () -> Files.readAllLines(g));
        r("rs", () -> Files.readString(g));
        r("ex", () -> Files.exists(f));
        r("nex", () -> Files.notExists(f));
        r("isd", () -> Files.isDirectory(d));
        r("isr", () -> Files.isRegularFile(f));
        r("isrd", () -> Files.isReadable(f));
        r("isw", () -> Files.isWritable(f));
        r("isx", () -> Files.isExecutable(f));
        r("ish", () -> Files.isHidden(f));
        r("sz", () -> Files.size(f));
        r("sl", () -> Files.isSameFile(f, f));
        r("cp", () -> Files.copy(f, tmp.resolve("c.txt")));
        r("cp2", () -> Files.copy(f, tmp.resolve("c.txt"), StandardCopyOption.REPLACE_EXISTING));
        r("mv", () -> Files.move(tmp.resolve("c.txt"), tmp.resolve("d.txt")));
        r("del", () -> Files.delete(tmp.resolve("d.txt")));
        r("dele", () -> Files.deleteIfExists(tmp.resolve("d.txt")));
        r("nis", () -> { try (InputStream in = Files.newInputStream(f)) { in.read(); } });
        r("nos", () -> { try (OutputStream o = Files.newOutputStream(tmp.resolve("e.txt"))) { o.write(1); } });
        r("nbr", () -> { try (BufferedReader br = Files.newBufferedReader(g)) { br.readLine(); } });
        r("nbw", () -> { try (BufferedWriter bw = Files.newBufferedWriter(tmp.resolve("f.txt"))) { bw.write("z"); } });
        r("ds", () -> { try (DirectoryStream<Path> s = Files.newDirectoryStream(tmp)) { for (Path p : s) p.getFileName(); } });
        r("walk", () -> Files.walk(tmp).count());
        r("walk1", () -> Files.walk(tmp, 1).count());
        r("list", () -> Files.list(tmp).count());
        r("find", () -> Files.find(tmp, 2, (p, at) -> true).count());
        r("lines", () -> Files.lines(g).count());
        r("attr", () -> Files.readAttributes(f, BasicFileAttributes.class));
        r("attr2", () -> Files.getAttribute(f, "basic:size"));
        r("lmt", () -> Files.getLastModifiedTime(f));
        r("slmt", () -> Files.setLastModifiedTime(f, FileTime.fromMillis(1000000L)));
        r("prb", () -> Files.probeContentType(f));
        r("store", () -> Files.getFileStore(f));
        r("tmpf", () -> Files.createTempFile("l4", ".tmp"));
        r("tmpd", () -> Files.createTempDirectory("l4d"));
        r("ap", () -> Files.write(g, "more".getBytes(), StandardOpenOption.APPEND));
        r("cpio", () -> Files.copy(new ByteArrayInputStream(new byte[]{7}), tmp.resolve("h.txt")));
        r("cpout", () -> Files.copy(f, new ByteArrayOutputStream()));
        r("mism", () -> Files.mismatch(f, g));
        r("wlt", () -> Files.walkFileTree(tmp, new SimpleFileVisitor<Path>() {}));
    }

    static void paths() {
        Path p = Paths.get("a", "b", "c");
        r("gp", () -> p.getParent());
        r("gf", () -> p.getFileName());
        r("gr", () -> p.getRoot());
        r("nc", () -> p.getNameCount());
        r("gn", () -> p.getName(0));
        r("sp", () -> p.subpath(0, 2));
        r("nrm", () -> Paths.get("a/./b/../c").normalize());
        r("abs", () -> p.toAbsolutePath());
        r("isa", () -> p.isAbsolute());
        r("rslv", () -> p.resolve("d"));
        r("rslvs", () -> p.resolveSibling("d"));
        r("rel", () -> p.relativize(Paths.get("a", "b", "c", "d")));
        r("sw", () -> p.startsWith("a"));
        r("ew", () -> p.endsWith("c"));
        r("tf", () -> p.toFile());
        r("turi", () -> p.toUri());
        r("cmp", () -> p.compareTo(Paths.get("a")));
        r("eq", () -> p.equals(Paths.get("a", "b", "c")));
        r("it", () -> { for (Path q : p) q.toString(); });
        r("of", () -> Path.of("x", "y"));
        r("fs", () -> FileSystems.getDefault());
        r("gpth", () -> FileSystems.getDefault().getPath("q"));
        r("seps", () -> FileSystems.getDefault().getSeparator());
        r("roots", () -> { for (Path q : FileSystems.getDefault().getRootDirectories()) q.toString(); });
        r("real", () -> tmp.toRealPath());
    }

    static void file() throws Exception {
        File t = File.createTempFile("l4f", ".tmp");
        File dir = t.getParentFile();
        r("gn", () -> t.getName());
        r("gpp", () -> t.getParent());
        r("gpf", () -> t.getParentFile());
        r("gpath", () -> t.getPath());
        r("isa", () -> t.isAbsolute());
        r("gap", () -> t.getAbsolutePath());
        r("gaf", () -> t.getAbsoluteFile());
        r("gcp", () -> t.getCanonicalPath());
        r("gcf", () -> t.getCanonicalFile());
        r("uri", () -> t.toURI());
        r("cr", () -> t.canRead());
        r("cw", () -> t.canWrite());
        r("cx", () -> t.canExecute());
        r("ex", () -> t.exists());
        r("isd", () -> t.isDirectory());
        r("isf", () -> t.isFile());
        r("ish", () -> t.isHidden());
        r("lm", () -> t.lastModified());
        r("len", () -> t.length());
        r("cnf", () -> new File(dir, "l4-new.tmp").createNewFile());
        r("del", () -> new File(dir, "l4-new.tmp").delete());
        r("doe", () -> t.deleteOnExit());
        r("list", () -> dir.list());
        r("listf", () -> dir.listFiles());
        r("listff", () -> dir.listFiles((FileFilter) x -> true));
        r("listfn", () -> dir.listFiles((FilenameFilter) (x, n) -> true));
        r("mkdir", () -> new File(dir, "l4d1").mkdir());
        r("mkdirs", () -> new File(dir, "l4d2/x/y").mkdirs());
        r("ren", () -> t.renameTo(new File(dir, "l4-ren.tmp")));
        r("ren2", () -> new File(dir, "l4-ren.tmp").renameTo(t));
        r("slm", () -> t.setLastModified(1000000L));
        r("sro", () -> t.setReadOnly());
        r("sw", () -> t.setWritable(true));
        r("sr", () -> t.setReadable(true));
        r("sx", () -> t.setExecutable(true));
        r("swo", () -> t.setWritable(true, true));
        r("tsp", () -> t.getTotalSpace());
        r("fsp", () -> t.getFreeSpace());
        r("usp", () -> t.getUsableSpace());
        r("cmp", () -> t.compareTo(new File("a")));
        r("eq", () -> t.equals(new File(t.getPath())));
        r("hc", () -> t.hashCode());
        r("ts", () -> t.toString());
        r("tp", () -> t.toPath());
        r("roots", () -> File.listRoots());
        r("sep", () -> { String s = File.separator; });
        r("sepc", () -> { char c = File.separatorChar; });
        r("psep", () -> { String s = File.pathSeparator; });
        r("ctf", () -> File.createTempFile("l4x", ".t", dir));
        r("ctor2", () -> new File("a", "b"));
        r("ctor3", () -> new File(new File("a"), "b"));
        r("ctoruri", () -> new File(t.toURI()));
        r("cleanup", () -> { t.delete(); new File(dir, "l4d1").delete(); });
    }

    static void byteBuffers() {
        ByteBuffer h = ByteBuffer.allocate(32);
        ByteBuffer d = ByteBuffer.allocateDirect(32);
        ByteBuffer w = ByteBuffer.wrap(new byte[16]);
        for (ByteBuffer b : new ByteBuffer[]{h, d, w}) {
            r("cap", () -> b.capacity());
            r("pos", () -> b.position());
            r("lim", () -> b.limit());
            r("rem", () -> b.remaining());
            r("hr", () -> b.hasRemaining());
            r("ro", () -> b.isReadOnly());
            r("dir", () -> b.isDirect());
            r("ha", () -> b.hasArray());
            r("ord", () -> b.order());
            r("put", () -> b.put((byte) 1));
            r("putb", () -> b.put(new byte[]{2, 3}));
            r("putbo", () -> b.put(new byte[]{4, 5, 6}, 1, 2));
            r("puti", () -> b.putInt(7));
            r("putl", () -> b.putLong(8L));
            r("puts", () -> b.putShort((short) 9));
            r("putc", () -> b.putChar('z'));
            r("putf", () -> b.putFloat(1.5f));
            r("putd", () -> b.putDouble(2.5));
            r("flip", () -> b.flip());
            r("get", () -> b.get());
            r("geta", () -> b.get(new byte[2]));
            r("getao", () -> b.get(new byte[4], 1, 2));
            r("geti", () -> b.getInt());
            r("getl", () -> b.getLong());
            r("gets", () -> b.getShort());
            r("getc", () -> b.getChar());
            r("getf", () -> b.getFloat());
            r("getd", () -> b.getDouble());
            r("absget", () -> b.get(0));
            r("absput", () -> b.put(0, (byte) 1));
            r("absgeti", () -> b.getInt(0));
            r("absputi", () -> b.putInt(0, 1));
            r("mark", () -> b.mark());
            r("reset", () -> b.reset());
            r("rew", () -> b.rewind());
            r("clr", () -> b.clear());
            r("slice", () -> b.slice());
            r("dup", () -> b.duplicate());
            r("asro", () -> b.asReadOnlyBuffer());
            r("cmp", () -> b.compareTo(ByteBuffer.allocate(1)));
            r("eq", () -> b.equals(ByteBuffer.allocate(1)));
            r("hc", () -> b.hashCode());
            r("ts", () -> b.toString());
            r("ordset", () -> b.order(ByteOrder.LITTLE_ENDIAN));
            r("aib", () -> b.asIntBuffer());
            r("acb", () -> b.asCharBuffer());
            r("alb", () -> b.asLongBuffer());
            r("adb", () -> b.asDoubleBuffer());
            r("afb", () -> b.asFloatBuffer());
            r("asb", () -> b.asShortBuffer());
            r("arr", () -> b.array());
            r("aoff", () -> b.arrayOffset());
            r("compact", () -> b.compact());
            r("slice2", () -> b.slice(0, 4));
            r("dupput", () -> ByteBuffer.allocate(8).put(ByteBuffer.allocate(4)));
        }
        r("wrapoff", () -> ByteBuffer.wrap(new byte[8], 2, 4));
        r("cb", () -> CharBuffer.wrap("hello"));
        r("cba", () -> CharBuffer.allocate(4));
        r("ib", () -> IntBuffer.allocate(4));
        r("lb", () -> LongBuffer.allocate(4));
        r("sb", () -> ShortBuffer.allocate(4));
        r("fb", () -> FloatBuffer.allocate(4));
        r("db", () -> DoubleBuffer.allocate(4));
        r("enc", () -> StandardCharsets.UTF_8.encode("hi"));
        r("dec", () -> StandardCharsets.UTF_8.decode(ByteBuffer.wrap("hi".getBytes())));
    }

    static void printStreams() throws Exception {
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        PrintStream ps = new PrintStream(sink, true, "UTF-8");
        r("pb", () -> ps.print(true));
        r("pc", () -> ps.print('c'));
        r("pi", () -> ps.print(1));
        r("pl", () -> ps.print(2L));
        r("pf", () -> ps.print(3.0f));
        r("pd", () -> ps.print(4.0));
        r("pca", () -> ps.print(new char[]{'a'}));
        r("pstr", () -> ps.print("s"));
        r("po", () -> ps.print((Object) "o"));
        r("pln", () -> ps.println());
        r("plnb", () -> ps.println(true));
        r("plnc", () -> ps.println('c'));
        r("plni", () -> ps.println(1));
        r("plnl", () -> ps.println(2L));
        r("plnf", () -> ps.println(3.0f));
        r("plnd", () -> ps.println(4.0));
        r("plnca", () -> ps.println(new char[]{'a'}));
        r("plns", () -> ps.println("s"));
        r("plno", () -> ps.println((Object) "o"));
        r("wi", () -> ps.write(65));
        r("wb", () -> ps.write(new byte[]{66, 67}, 0, 2));
        r("wbs", () -> ps.write(new byte[]{68}));
        r("wbb", () -> ps.write(new byte[]{102,103}));
        r("fmt", () -> ps.format("%d-%s%n", 1, "x"));
        r("prf", () -> ps.printf("%s", "y"));
        r("prfl", () -> ps.printf(Locale.ROOT, "%s", "y"));
        r("ap", () -> ps.append('a'));
        r("aps", () -> ps.append("bc"));
        r("apr", () -> ps.append("bcd", 1, 2));
        r("flush", () -> ps.flush());
        r("ce", () -> ps.checkError());
        r("cs", () -> ps.charset());
        r("close", () -> ps.close());
        r("ce2", () -> ps.checkError());
        r("ctorFile", () -> new PrintStream(File.createTempFile("l4ps", ".t")).close());
        r("ctorName", () -> new PrintStream(File.createTempFile("l4ps2", ".t").getPath()).close());
        r("ctorCs", () -> new PrintStream(new ByteArrayOutputStream(), true, StandardCharsets.UTF_8).close());
        r("pw", () -> { PrintWriter p = new PrintWriter(new StringWriter()); p.println("x"); p.flush(); p.close(); });
    }

    static void ioTail() throws Exception {
        r("bais", () -> { ByteArrayInputStream b = new ByteArrayInputStream(new byte[]{1, 2, 3}); b.read(); b.skip(1); b.available(); b.mark(1); b.reset(); b.readAllBytes(); b.close(); });
        r("baos", () -> { ByteArrayOutputStream b = new ByteArrayOutputStream(); b.write(1); b.write(new byte[]{2}, 0, 1); b.toByteArray(); b.size(); b.reset(); b.toString(); b.close(); });
        r("bis", () -> { BufferedInputStream b = new BufferedInputStream(new ByteArrayInputStream(new byte[]{1, 2, 3, 4})); b.read(); b.mark(4); b.read(new byte[2]); b.reset(); b.skip(1); b.available(); b.close(); });
        r("bos", () -> { BufferedOutputStream b = new BufferedOutputStream(new ByteArrayOutputStream()); b.write(1); b.write(new byte[]{2}, 0, 1); b.flush(); b.close(); });
        r("dis", () -> { DataInputStream b = new DataInputStream(new ByteArrayInputStream(new byte[32])); b.readInt(); b.readLong(); b.readShort(); b.readByte(); b.readBoolean(); b.readUnsignedByte(); b.readFully(new byte[2]); b.close(); });
        r("dos", () -> { DataOutputStream b = new DataOutputStream(new ByteArrayOutputStream()); b.writeInt(1); b.writeLong(2); b.writeShort(3); b.writeByte(4); b.writeBoolean(true); b.writeUTF("x"); b.writeChars("y"); b.writeBytes("z"); b.size(); b.flush(); b.close(); });
        r("isr", () -> { InputStreamReader r2 = new InputStreamReader(new ByteArrayInputStream("hi".getBytes()), StandardCharsets.UTF_8); r2.read(); r2.read(new char[2]); r2.ready(); r2.getEncoding(); r2.close(); });
        r("osw", () -> { OutputStreamWriter w = new OutputStreamWriter(new ByteArrayOutputStream(), StandardCharsets.UTF_8); w.write('a'); w.write("bc"); w.write(new char[]{'d'}); w.append('e'); w.flush(); w.close(); });
        r("br", () -> { BufferedReader b = new BufferedReader(new StringReader("l1\nl2\n")); b.readLine(); b.read(); b.lines().count(); b.close(); });
        r("bw", () -> { BufferedWriter b = new BufferedWriter(new StringWriter()); b.write("x"); b.newLine(); b.flush(); b.close(); });
        r("sr", () -> { StringReader s = new StringReader("abc"); s.read(); s.skip(1); s.markSupported(); s.mark(1); s.reset(); s.close(); });
        r("sw", () -> { StringWriter s = new StringWriter(); s.write("a"); s.append('b'); s.getBuffer(); s.toString(); s.close(); });
        r("cw", () -> { CharArrayWriter c = new CharArrayWriter(); c.write('a'); c.toCharArray(); c.size(); c.reset(); c.close(); });
        r("cr", () -> { CharArrayReader c = new CharArrayReader(new char[]{'a', 'b'}); c.read(); c.close(); });
        r("fos", () -> { File t = File.createTempFile("l4fo", ".t"); FileOutputStream o = new FileOutputStream(t); o.write(1); o.write(new byte[]{2}); o.getFD(); o.getChannel(); o.flush(); o.close(); FileInputStream i = new FileInputStream(t); i.read(); i.available(); i.getFD(); i.getChannel(); i.skip(0); i.close(); t.delete(); });
        r("raf", () -> { File t = File.createTempFile("l4raf", ".t"); RandomAccessFile f = new RandomAccessFile(t, "rw"); f.writeInt(7); f.seek(0); f.readInt(); f.length(); f.getFilePointer(); f.setLength(8); f.getChannel(); f.close(); t.delete(); });
        r("pshr", () -> { PushbackInputStream p = new PushbackInputStream(new ByteArrayInputStream(new byte[]{1, 2})); p.read(); p.unread(1); p.read(); p.close(); });
        r("seq", () -> { SequenceInputStream s = new SequenceInputStream(new ByteArrayInputStream(new byte[]{1}), new ByteArrayInputStream(new byte[]{2})); s.read(); s.read(); s.close(); });
        r("scan", () -> { Scanner s = new Scanner("1 two 3.0"); s.nextInt(); s.next(); s.nextDouble(); s.close(); });
        r("obj", () -> { ByteArrayOutputStream b = new ByteArrayOutputStream(); ObjectOutputStream o = new ObjectOutputStream(b); o.writeObject("s"); o.writeInt(1); o.flush(); o.close(); ObjectInputStream i = new ObjectInputStream(new ByteArrayInputStream(b.toByteArray())); i.readObject(); i.readInt(); i.close(); });
        r("nullis", () -> { InputStream i = InputStream.nullInputStream(); i.read(); i.close(); });
        r("nullos", () -> { OutputStream o = OutputStream.nullOutputStream(); o.write(1); o.close(); });
        r("transfer", () -> new ByteArrayInputStream(new byte[]{1, 2}).transferTo(new ByteArrayOutputStream()));
        r("nullrd", () -> { Reader rr = Reader.nullReader(); rr.read(); rr.close(); });
        r("nullwr", () -> { Writer ww = Writer.nullWriter(); ww.write("x"); ww.close(); });
    }

    static void nioTail() throws Exception {
        Path f = tmp.resolve("chan.bin");
        r("fc", () -> { try (FileChannel c = FileChannel.open(f, StandardOpenOption.CREATE, StandardOpenOption.WRITE, StandardOpenOption.READ)) {
            c.write(ByteBuffer.wrap(new byte[]{1, 2, 3}));
            c.position(0);
            c.read(ByteBuffer.allocate(3));
            c.size(); c.force(false); c.truncate(2); c.position();
        } });
        r("map", () -> { try (FileChannel c = FileChannel.open(f, StandardOpenOption.READ)) {
            c.map(FileChannel.MapMode.READ_ONLY, 0, Math.max(1, c.size()));
        } });
        r("chans", () -> Channels.newInputStream(FileChannel.open(f, StandardOpenOption.READ)).close());
        r("cs", () -> { Charset c = StandardCharsets.UTF_8; c.name(); c.displayName(); c.canEncode(); c.newEncoder(); c.newDecoder(); c.aliases(); });
        r("csfn", () -> Charset.forName("UTF-8"));
        r("csdef", () -> Charset.defaultCharset());
        r("csav", () -> Charset.isSupported("US-ASCII"));
        r("enc2", () -> StandardCharsets.ISO_8859_1.encode(CharBuffer.wrap("hi")));
        r("bo", () -> ByteOrder.nativeOrder());
    }

    public static void main(String[] a) {
        try { files(); } catch (Throwable e) { }
        try { paths(); } catch (Throwable e) { }
        try { file(); } catch (Throwable e) { }
        try { byteBuffers(); } catch (Throwable e) { }
        try { printStreams(); } catch (Throwable e) { }
        try { ioTail(); } catch (Throwable e) { }
        try { nioTail(); } catch (Throwable e) { }
        System.out.println("L4Reach ok=" + ok + " threw=" + threw);
        System.out.println("DONE L4Reach");
    }
}
