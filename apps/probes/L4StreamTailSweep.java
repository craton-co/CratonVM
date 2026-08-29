import java.io.*;
import java.nio.*;
import java.nio.charset.*;
import java.nio.file.attribute.FileTime;
import java.util.*;
import java.util.concurrent.TimeUnit;

/** L4 -- the `java.io` stream tail and the `java.nio` tail, at their edges.
 *
 *  These are the remaining `outcome=native-won` triples in the two packages
 *  after `File`, `Files`, `ByteBuffer` and `PrintStream`:
 *
 *      DataOutputStream 10   ByteArrayInputStream 7   BufferedWriter 5
 *      DataInputStream   8   FileOutputStream     5   BufferedOutputStream 4
 *      FilterInputStream 1   FileDescriptor       1
 *      CoderResult 2   ByteOrder 1   CharBuffer 1   FileTime 1
 *
 *  The edges that decide a stream shim, none of which the happy path reaches:
 *
 *   * `ByteArrayInputStream.close()` is a NO-OP -- the stream stays readable.
 *     A shim that marks it closed and then refuses is wrong in the direction
 *     that looks careful.
 *   * `read(b, 0, 0)` at EOF answers **0**, not -1. The zero-length read is
 *     specified to return before the EOF test.
 *   * every `DataInputStream.readX` at end of stream is `EOFException`, which
 *     is an `IOException` subtype a caller catches SEPARATELY to mean "the
 *     record ended", so answering a plain `IOException` breaks the loop shape
 *     the class exists for.
 *   * a `BufferedWriter` write AFTER close is an `IOException`, while a
 *     `BufferedOutputStream` close is idempotent.
 */
public class L4StreamTailSweep {
    static int rows = 0;
    static String esc(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '\n') b.append("\\n");
            else if (c == '\r') b.append("\\r");
            else if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        rows++;
        try { r.run(); System.out.println(esc(tag) + " |no-throw|"); }
        catch (Throwable e) { System.out.println(esc(tag) + " |THREW " + e.getClass().getName() + "|"); }
    }
    interface ThrowingRun { void run() throws Throwable; }
    static String hex(byte[] a) {
        if (a == null) return "null";
        StringBuilder s = new StringBuilder();
        for (byte x : a) s.append(String.format("%02x", x));
        return s.toString();
    }

    // ------------------------------------------------- ByteArrayInputStream

    static void bais() throws Exception {
        byte[] src = {10, 20, 30, 40, 50};
        ByteArrayInputStream b = new ByteArrayInputStream(src);
        p("available fresh", b.available());
        p("read", b.read());
        p("available after read", b.available());
        p("markSupported", b.markSupported());
        b.mark(2);
        p("read after mark", b.read());
        b.reset();
        p("read after reset", b.read());
        byte[] into = new byte[3];
        p("read(b,0,2)", b.read(into, 0, 2));
        p("read content", hex(into));
        p("skip(1)", b.skip(1));
        p("available after skip", b.available());
        p("read at end", b.read());
        p("read(b,0,1) at end", b.read(into, 0, 1));
        // A ZERO-LENGTH read at EOF answers 0, not -1.
        p("read(b,0,0) at end", b.read(into, 0, 0));
        p("skip at end", b.skip(4));
        // A NEGATIVE skip is 0, not an exception and not a rewind.
        b.reset();
        p("skip(-1)", b.skip(-1));
        p("position kept after skip(-1)", b.read());
        p("skip past end returns remaining", b.skip(1000));
        // close is a NO-OP: the stream is still readable afterwards.
        ByteArrayInputStream c = new ByteArrayInputStream(src);
        c.close();
        p("read after close", c.read());
        p("available after close", c.available());
        t("reset after close", () -> c.reset());
        // reset with NO mark goes back to the constructor's offset, which is
        // 0 for the two-argument form -- it does not throw.
        ByteArrayInputStream off = new ByteArrayInputStream(src, 2, 2);
        p("offset available", off.available());
        p("offset read", off.read());
        off.reset();
        p("offset reset goes to offset", off.read());
        p("offset read past its limit", (off.read() != -1) + "/" + "second");
        ByteArrayInputStream o2 = new ByteArrayInputStream(src, 2, 2);
        o2.read(); o2.read();
        p("offset stream ends at limit", o2.read());
        // Constructor argument checking.
        t("new BAIS(null)", () -> new ByteArrayInputStream(null));
        t("new BAIS(null,0,0)", () -> new ByteArrayInputStream(null, 0, 0));
        p("new BAIS(src,-1,2) available", boolOr(() -> new ByteArrayInputStream(src, -1, 2).available()));
        p("new BAIS(src,0,99) available", boolOr(() -> new ByteArrayInputStream(src, 0, 99).available()));
        // Bulk-read argument checking.
        ByteArrayInputStream d = new ByteArrayInputStream(src);
        t("read(null,0,1)", () -> d.read(null, 0, 1));
        t("read(b,-1,1)", () -> d.read(into, -1, 1));
        t("read(b,0,-1)", () -> d.read(into, 0, -1));
        t("read(b,0,9) past end", () -> d.read(into, 0, 9));
        t("read(b,1,MAX)", () -> d.read(into, 1, Integer.MAX_VALUE));
        p("state unchanged after refusals", d.available());
        p("readAllBytes", hex(new ByteArrayInputStream(src).readAllBytes()));
        p("readNBytes(2)", hex(new ByteArrayInputStream(src).readNBytes(2)));
        t("readNBytes(-1)", () -> new ByteArrayInputStream(src).readNBytes(-1));
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        p("transferTo", new ByteArrayInputStream(src).transferTo(sink));
        p("transferTo content", hex(sink.toByteArray()));
    }

    static boolean boolOr(ThrowingRun r) {
        try { r.run(); return true; } catch (Throwable e) { return false; }
    }

    // ----------------------------------------------------- Data{In,Out}put

    static void data() throws Exception {
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        DataOutputStream o = new DataOutputStream(sink);
        p("size fresh", o.size());
        o.writeBoolean(true);
        o.writeBoolean(false);
        o.writeByte(0x7F);
        o.writeByte(0x1FF);
        o.writeShort(0x1234);
        o.writeShort(0x1FFFF);
        o.writeInt(0x01020304);
        o.writeLong(0x0102030405060708L);
        o.writeChar('A');
        o.writeFloat(1.5f);
        o.writeDouble(2.5);
        o.writeUTF("héllo");
        o.writeChars("ab");
        o.writeBytes("cd");
        o.flush();
        p("size after writes", o.size());
        p("bytes", hex(sink.toByteArray()));

        DataInputStream i = new DataInputStream(new ByteArrayInputStream(sink.toByteArray()));
        p("readBoolean true", i.readBoolean());
        p("readBoolean false", i.readBoolean());
        p("readByte", i.readByte());
        p("readByte truncated", i.readByte());
        p("readShort", i.readShort());
        p("readShort truncated", i.readShort());
        p("readInt", i.readInt());
        p("readLong", i.readLong());
        p("readChar", i.readChar());
        p("readFloat", i.readFloat());
        p("readDouble", i.readDouble());
        p("readUTF", i.readUTF());
        p("readChar a", i.readChar());
        p("readChar b", i.readChar());
        p("readUnsignedByte c", i.readUnsignedByte());
        p("readUnsignedByte d", i.readUnsignedByte());
        p("available at end", i.available());
        // Every read at end of stream is EOFException, not -1 and not IOException.
        t("readBoolean at EOF", () -> i.readBoolean());
        t("readByte at EOF", () -> i.readByte());
        t("readUnsignedByte at EOF", () -> i.readUnsignedByte());
        t("readShort at EOF", () -> i.readShort());
        t("readInt at EOF", () -> i.readInt());
        t("readLong at EOF", () -> i.readLong());
        t("readChar at EOF", () -> i.readChar());
        t("readFloat at EOF", () -> i.readFloat());
        t("readUTF at EOF", () -> i.readUTF());
        // read() -- the InputStream method -- answers -1 rather than throwing.
        p("read() at EOF", i.read());
        p("read(b,0,1) at EOF", i.read(new byte[1], 0, 1));
        // A PARTIAL record is also EOFException, and readFully must not
        // half-fill and report success.
        DataInputStream sh = new DataInputStream(new ByteArrayInputStream(new byte[]{1, 2}));
        t("readInt over 2 bytes", () -> sh.readInt());
        DataInputStream sh2 = new DataInputStream(new ByteArrayInputStream(new byte[]{1, 2}));
        byte[] four = new byte[4];
        t("readFully(4) over 2 bytes", () -> sh2.readFully(four));
        DataInputStream sh3 = new DataInputStream(new ByteArrayInputStream(new byte[]{1, 2, 3, 4}));
        sh3.readFully(four);
        p("readFully content", hex(four));
        t("readFully(null)", () -> sh3.readFully(null));
        t("readFully(b,-1,1)", () -> sh3.readFully(four, -1, 1));
        t("readFully(b,0,9)", () -> sh3.readFully(four, 0, 9));
        p("readFully(b,0,0) at EOF", boolOr(() -> sh3.readFully(four, 0, 0)));
        t("readFully(b,0,1) at EOF", () -> sh3.readFully(four, 0, 1));
        p("skipBytes past end", sh3.skipBytes(10));
        p("skipBytes negative", sh3.skipBytes(-1));
        // A malformed modified-UTF8 record is UTFDataFormatException.
        t("readUTF malformed", () -> new DataInputStream(
                new ByteArrayInputStream(new byte[]{0, 2, (byte) 0xC0, 0x00})).readUTF());
        t("readUTF truncated length", () -> new DataInputStream(
                new ByteArrayInputStream(new byte[]{0, 5, 65})).readUTF());
        // writeUTF of a string longer than 65535 UTF-8 bytes is refused.
        StringBuilder big = new StringBuilder();
        for (int k = 0; k < 70000; k++) big.append('x');
        t("writeUTF too long", () -> new DataOutputStream(new ByteArrayOutputStream()).writeUTF(big.toString()));
        t("writeUTF(null)", () -> new DataOutputStream(new ByteArrayOutputStream()).writeUTF(null));
        p("writeUTF empty", hexOf(w -> w.writeUTF("")));
        // Modified UTF-8 encodes NUL as two bytes -- the one place it differs
        // from real UTF-8, and the row a shim written against UTF-8 gets wrong.
        p("writeUTF embedded NUL", hexOf(w -> w.writeUTF("a b")));
        p("writeUTF supplementary", hexOf(w -> w.writeUTF("😀")));
        // close, flush, size on the output side.
        DataOutputStream cl = new DataOutputStream(new ByteArrayOutputStream());
        cl.writeInt(1);
        p("size before close", cl.size());
        cl.close();
        t("close twice", () -> cl.close());
        t("write after close", () -> cl.writeInt(2));
        p("size after close", cl.size());
        DataInputStream ic = new DataInputStream(new ByteArrayInputStream(new byte[]{1}));
        ic.close();
        p("BAIS-backed read after close", ic.read());
        // A null underlying stream is accepted by the constructor and only
        // fails on use.
        t("new DataInputStream(null)", () -> new DataInputStream(null));
        t("new DataOutputStream(null)", () -> new DataOutputStream(null));
        t("use of DataOutputStream(null)", () -> new DataOutputStream(null).writeInt(1));
    }

    interface W { void run(DataOutputStream o) throws IOException; }
    static String hexOf(W w) throws IOException {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        DataOutputStream o = new DataOutputStream(b);
        w.run(o);
        o.flush();
        return hex(b.toByteArray());
    }

    // ---------------------------------------------------- buffered wrappers

    static void buffered() throws Exception {
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        BufferedOutputStream b = new BufferedOutputStream(sink, 4);
        b.write(1);
        p("buffered write is not through yet", sink.size());
        b.write(new byte[]{2, 3});
        p("still buffered", sink.size());
        b.write(4);
        p("full buffer flushes", sink.size());
        b.flush();
        p("after flush", hex(sink.toByteArray()));
        // A write LARGER than the buffer goes straight through.
        ByteArrayOutputStream s2 = new ByteArrayOutputStream();
        BufferedOutputStream b2 = new BufferedOutputStream(s2, 4);
        b2.write(new byte[]{1, 2, 3, 4, 5, 6});
        p("oversized write passes through", s2.size());
        b2.close();
        p("close flushed", s2.size());
        t("close twice", () -> b2.close());
        t("write after close", () -> b2.write(1));
        t("flush after close", () -> b2.flush());
        t("new BufferedOutputStream(os,0)", () -> new BufferedOutputStream(new ByteArrayOutputStream(), 0));
        t("new BufferedOutputStream(os,-1)", () -> new BufferedOutputStream(new ByteArrayOutputStream(), -1));
        t("new BufferedOutputStream(null)", () -> new BufferedOutputStream(null));
        BufferedOutputStream b3 = new BufferedOutputStream(new ByteArrayOutputStream(), 4);
        t("write(null,0,1)", () -> b3.write(null, 0, 1));
        t("write(b,-1,1)", () -> b3.write(new byte[2], -1, 1));
        t("write(b,0,9)", () -> b3.write(new byte[2], 0, 9));
        t("write(b,1,MAX)", () -> b3.write(new byte[2], 1, Integer.MAX_VALUE));

        StringWriter sw = new StringWriter();
        BufferedWriter w = new BufferedWriter(sw, 4);
        w.write("ab");
        p("writer buffered", sw.toString().length());
        w.write("cdef");
        w.flush();
        p("writer after flush", sw.toString());
        w.newLine();
        w.flush();
        p("newLine is the platform separator", sw.toString());
        w.write(new char[]{'x', 'y', 'z'}, 1, 2);
        w.write("pqrs", 1, 2);
        w.write('!');
        w.flush();
        p("writer subranges", sw.toString());
        t("write([C,-1,1)", () -> w.write(new char[2], -1, 1));
        t("write([C,0,9)", () -> w.write(new char[2], 0, 9));
        t("write([C,1,MAX)", () -> w.write(new char[2], 1, Integer.MAX_VALUE));
        t("write((char[])null,0,1)", () -> w.write((char[]) null, 0, 1));
        t("write(String,-1,1)", () -> w.write("ab", -1, 1));
        t("write(String,0,9)", () -> w.write("ab", 0, 9));
        t("write((String)null,0,1)", () -> w.write((String) null, 0, 1));
        w.close();
        t("writer close twice", () -> w.close());
        t("write after writer close", () -> w.write("z"));
        t("newLine after writer close", () -> w.newLine());
        t("flush after writer close", () -> w.flush());
        t("new BufferedWriter(w,0)", () -> new BufferedWriter(new StringWriter(), 0));
        t("new BufferedWriter(null)", () -> new BufferedWriter(null));
        p("writer final content", sw.toString());
    }

    // ------------------------------------------------------- FileOutputStream

    static void fileStreams() throws Exception {
        File f = new File("l4tail.bin");
        try {
            FileOutputStream o = new FileOutputStream(f);
            o.write(1);
            o.write(new byte[]{2, 3});
            o.write(new byte[]{4, 5, 6}, 1, 2);
            o.flush();
            p("file length after writes", f.length());
            t("write(null)", () -> o.write((byte[]) null));
            t("write(null,0,1)", () -> o.write(null, 0, 1));
            t("write(b,-1,1)", () -> o.write(new byte[2], -1, 1));
            t("write(b,0,9)", () -> o.write(new byte[2], 0, 9));
            t("write(b,1,MAX)", () -> o.write(new byte[2], 1, Integer.MAX_VALUE));
            p("write(b,0,0) is legal", boolOr(() -> o.write(new byte[2], 0, 0)));
            p("fd valid", o.getFD().valid());
            o.close();
            t("close twice", () -> o.close());
            t("write after close", () -> o.write(1));
            t("flush after close", () -> o.flush());
            p("fd valid after close", o.getFD().valid());
            p("content", hex(java.nio.file.Files.readAllBytes(f.toPath())));
            // append vs truncate.
            try (FileOutputStream ap = new FileOutputStream(f, true)) { ap.write(9); }
            p("append kept prefix", hex(java.nio.file.Files.readAllBytes(f.toPath())));
            try (FileOutputStream tr = new FileOutputStream(f)) { tr.write(8); }
            p("truncate dropped prefix", hex(java.nio.file.Files.readAllBytes(f.toPath())));
            t("open a directory for write", () -> new FileOutputStream(new File(".")).close());
            t("open in missing directory", () -> new FileOutputStream(new File("l4-nope/x")).close());
            t("new FileOutputStream((File)null)", () -> new FileOutputStream((File) null));
            t("new FileOutputStream((String)null)", () -> new FileOutputStream((String) null));
            // FileInputStream's own EOF and close rules, for the pair.
            FileInputStream in = new FileInputStream(f);
            p("read", in.read());
            p("read at EOF", in.read());
            p("read(b,0,0) at EOF", in.read(new byte[1], 0, 0));
            p("available at EOF", in.available());
            p("skip at EOF", in.skip(4));
            in.close();
            t("read after close", () -> in.read());
            t("available after close", () -> in.available());
            t("new FileInputStream(missing)", () -> new FileInputStream(new File("l4-nope.bin")));
            t("new FileInputStream(dir)", () -> new FileInputStream(new File(".")));
            // A raw FileDescriptor is not valid until a stream owns it.
            FileDescriptor fd = new FileDescriptor();
            p("fresh FileDescriptor valid", fd.valid());
            t("sync on fresh FileDescriptor", () -> fd.sync());
            p("FileDescriptor.in valid", FileDescriptor.in.valid());
            p("FileDescriptor.out valid", FileDescriptor.out.valid());
            p("FileDescriptor.err valid", FileDescriptor.err.valid());
        } finally { f.delete(); }
    }

    // ------------------------------------------------------ filter + nio tail

    static void tails() throws Exception {
        // FilterInputStream's constructor accepts null; the failure is deferred.
        t("new BufferedInputStream(null)", () -> new BufferedInputStream(null));
        t("read from BufferedInputStream(null)", () -> new BufferedInputStream(null).read());
        t("new PushbackInputStream(null)", () -> new PushbackInputStream(null));
        p("PushbackInputStream unread then read", pushback());

        // CoderResult, reached through a real encoder run.
        CharsetEncoder enc = StandardCharsets.US_ASCII.newEncoder();
        CharBuffer in = CharBuffer.wrap("ab");
        ByteBuffer small = ByteBuffer.allocate(1);
        CoderResult r1 = enc.encode(in, small, true);
        p("encode overflow isOverflow", r1.isOverflow());
        p("encode overflow isUnderflow", r1.isUnderflow());
        p("encode overflow isError", r1.isError());
        p("encode overflow toString", r1.toString());
        ByteBuffer big = ByteBuffer.allocate(8);
        CoderResult r2 = enc.encode(CharBuffer.wrap("ab"), big, true);
        p("encode ok isUnderflow", r2.isUnderflow());
        p("CoderResult.OVERFLOW is a singleton", CoderResult.OVERFLOW == r1);
        p("CoderResult.UNDERFLOW is a singleton", CoderResult.UNDERFLOW == r2);
        p("UNDERFLOW isError", CoderResult.UNDERFLOW.isError());
        t("UNDERFLOW length", () -> CoderResult.UNDERFLOW.length());
        t("UNDERFLOW throwException", () -> CoderResult.UNDERFLOW.throwException());
        CoderResult mal = CoderResult.malformedForLength(2);
        p("malformed isError", mal.isError());
        p("malformed isMalformed", mal.isMalformed());
        p("malformed isUnmappable", mal.isUnmappable());
        p("malformed length", mal.length());
        t("malformed throwException", () -> mal.throwException());
        t("malformedForLength(0)", () -> CoderResult.malformedForLength(0));
        CoderResult unm = CoderResult.unmappableForLength(1);
        p("unmappable isUnmappable", unm.isUnmappable());
        t("unmappable throwException", () -> unm.throwException());
        // The decode side, for the malformed-input path.
        CharsetDecoder dec = StandardCharsets.UTF_8.newDecoder();
        CoderResult r3 = dec.decode(ByteBuffer.wrap(new byte[]{(byte) 0xC3, 0x28}), CharBuffer.allocate(4), true);
        p("decode malformed isError", r3.isError());
        p("decode malformed isMalformed", r3.isMalformed());
        p("decode malformed length", r3.isError() ? r3.length() : -1);

        // ByteOrder and the typed buffer allocators.
        p("nativeOrder is one of the two", ByteOrder.nativeOrder() == ByteOrder.LITTLE_ENDIAN
                || ByteOrder.nativeOrder() == ByteOrder.BIG_ENDIAN);
        p("BIG_ENDIAN toString", ByteOrder.BIG_ENDIAN);
        p("LITTLE_ENDIAN toString", ByteOrder.LITTLE_ENDIAN);
        p("CharBuffer.allocate(4)", "p=" + CharBuffer.allocate(4).position() + " c=" + CharBuffer.allocate(4).capacity());
        t("CharBuffer.allocate(-1)", () -> CharBuffer.allocate(-1));
        p("CharBuffer.allocate(0) capacity", CharBuffer.allocate(0).capacity());
        p("CharBuffer.wrap toString", CharBuffer.wrap("hey").toString());
        p("CharBuffer put/flip", charRound());
        t("CharBuffer.wrap((CharSequence)null)", () -> CharBuffer.wrap((CharSequence) null));

        // FileTime is a value object with a documented ISO-8601 toString.
        FileTime ft = FileTime.fromMillis(1_000_000_000_000L);
        p("FileTime toMillis", ft.toMillis());
        p("FileTime toString", ft.toString());
        p("FileTime equals", ft.equals(FileTime.fromMillis(1_000_000_000_000L)));
        p("FileTime hashCode agrees", ft.hashCode() == FileTime.fromMillis(1_000_000_000_000L).hashCode());
        p("FileTime compareTo", Integer.signum(ft.compareTo(FileTime.fromMillis(0))));
        p("FileTime to seconds", ft.to(TimeUnit.SECONDS));
        p("FileTime zero", FileTime.fromMillis(0).toString());
        p("FileTime negative", FileTime.fromMillis(-1000).toString());
        p("FileTime from(seconds)", FileTime.from(1, TimeUnit.SECONDS).toMillis());
        p("FileTime toInstant", ft.toInstant().toString());
        p("FileTime equals other type", ft.equals("x"));
    }

    static String pushback() throws IOException {
        PushbackInputStream p2 = new PushbackInputStream(new ByteArrayInputStream(new byte[]{1, 2}));
        int a = p2.read();
        p2.unread(a);
        int b = p2.read();
        int c = p2.read();
        return a + "," + b + "," + c;
    }

    static String charRound() {
        CharBuffer c = CharBuffer.allocate(4);
        c.put('a').put('b');
        c.flip();
        return "p=" + c.position() + " l=" + c.limit() + " s=" + c.toString();
    }

    public static void main(String[] a) throws Exception {
        bais();
        data();
        buffered();
        fileStreams();
        tails();
        System.out.println("rows " + rows);
        System.out.println("DONE L4StreamTailSweep");
    }
}
