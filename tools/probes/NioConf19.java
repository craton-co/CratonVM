import java.io.*;
import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.charset.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;

/**
 * NIO cluster conformance facts (round 19, nio19): buffers, charsets, channels, selector,
 * HttpClient/HttpServer, SSLEngine/SSLSocket, WebSocket. Every line is a fact; no timings.
 * -Dnio19.rows=a,b selects rows.
 */
public class NioConf19 {
    static String rows = System.getProperty("nio19.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }

    static String ex(Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }
    interface Fact { Object get() throws Throwable; }
    static String f(Fact x) { try { return String.valueOf(x.get()); } catch (Throwable t) { return "!" + ex(t); } }

    public static void main(String[] a) throws Throwable {
        String[] all = {"bbequals", "bbcmp", "bbviews", "bbro", "bbbulk", "bbslice", "cbfacts", "charset",
                "coder", "filechan", "transfer", "mapped", "sockchan", "selector", "dgram", "http", "ssl",
                "sslsock", "ws", "pipe"};
        for (String r : all) {
            if (!on(r)) continue;
            try {
                NioConf19.class.getDeclaredMethod(r).invoke(null);
            } catch (java.lang.reflect.InvocationTargetException e) {
                System.out.println(r + " THREW " + ex(e.getCause()));
            }
        }
        System.out.println("done");
    }

    static void bbequals() {
        ByteBuffer d = ByteBuffer.allocateDirect(4);
        d.put(new byte[]{'a', 'b', 'c', 'd'}).flip();
        ByteBuffer h = ByteBuffer.wrap(new byte[]{'a', 'b', 'c', 'd'});
        System.out.println("bbequals " + d.equals("abcd") + "," + d.equals(Integer.valueOf(4)) + "," + h.equals("abcd")
                + "," + d.equals(h) + "," + h.equals(d) + "," + d.equals(null) + "," + h.equals(CharBuffer.wrap("abcd"))
                + "," + d.hashCode() + "," + h.hashCode() + "," + d.asReadOnlyBuffer().equals(h)
                + "," + ByteBuffer.allocate(0).equals(ByteBuffer.allocateDirect(0)));
        CharBuffer c = CharBuffer.wrap("abcd");
        System.out.println("cbequals " + c.equals("abcd") + "," + c.equals(CharBuffer.wrap(new char[]{'a', 'b', 'c', 'd'}))
                + "," + c.hashCode() + "," + c.compareTo(CharBuffer.wrap("abce")) + "," + c.mismatch(CharBuffer.wrap("abXd")));
        IntBuffer i = IntBuffer.wrap(new int[]{1, 2, 3});
        System.out.println("ibequals " + i.equals(new int[]{1, 2, 3}) + "," + i.equals(IntBuffer.wrap(new int[]{1, 2, 3}))
                + "," + i.hashCode() + "," + ByteBuffer.allocate(12).asIntBuffer().put(new int[]{1, 2, 3}).flip().equals(i));
        FloatBuffer fb = FloatBuffer.wrap(new float[]{Float.NaN, -0.0f});
        FloatBuffer fb2 = FloatBuffer.wrap(new float[]{Float.NaN, 0.0f});
        DoubleBuffer db = DoubleBuffer.wrap(new double[]{Double.NaN});
        System.out.println("fpequals " + fb.equals(fb2) + "," + fb.compareTo(fb2) + "," + db.equals(DoubleBuffer.wrap(new double[]{Double.NaN}))
                + "," + fb.mismatch(fb2));
    }

    static void bbcmp() {
        ByteBuffer x = ByteBuffer.wrap(new byte[]{1, 2, (byte) 0x80, 4});
        ByteBuffer y = ByteBuffer.allocateDirect(4).put(new byte[]{1, 2, 3, 4}).flip();
        System.out.println("bbcmp " + x.compareTo(y) + "," + y.compareTo(x) + "," + x.mismatch(y) + "," + y.mismatch(x)
                + "," + x.compareTo(x.duplicate()) + "," + ByteBuffer.allocate(3).mismatch(ByteBuffer.allocate(5))
                + "," + x.slice(1, 2).mismatch(y.slice(1, 2)));
        try { x.compareTo(null); System.out.println("bbcmp-null none"); } catch (Throwable t) { System.out.println("bbcmp-null " + t.getClass().getName()); }
    }

    static void bbviews() {
        ByteBuffer b = ByteBuffer.allocate(32);
        for (int i = 0; i < 32; i++) b.put(i, (byte) (i * 7 + 3));
        for (ByteOrder o : new ByteOrder[]{ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN}) {
            for (boolean direct : new boolean[]{false, true}) {
                ByteBuffer s = direct ? ByteBuffer.allocateDirect(32).put(b.duplicate()).flip() : b.duplicate();
                s.order(o);
                StringBuilder sb = new StringBuilder("bbviews " + o + "," + direct);
                sb.append(",c=").append((int) s.getChar(1)).append(",s=").append(s.getShort(3)).append(",i=").append(Integer.toHexString(s.getInt(5)))
                        .append(",l=").append(Long.toHexString(s.getLong(9))).append(",f=").append(s.getFloat(17)).append(",d=").append(s.getDouble(21));
                CharBuffer cv = s.position(1).asCharBuffer();
                IntBuffer iv = s.position(2).asIntBuffer();
                LongBuffer lv = s.position(3).asLongBuffer();
                sb.append(",cv=").append(cv.getClass().getSimpleName()).append(':').append(cv.remaining()).append(':').append((int) cv.get(2)).append(':').append(cv.isDirect()).append(':').append(cv.order());
                sb.append(",iv=").append(iv.getClass().getSimpleName()).append(':').append(iv.remaining()).append(':').append(Integer.toHexString(iv.get(1))).append(':').append(iv.isDirect()).append(':').append(iv.isReadOnly());
                sb.append(",lv=").append(lv.remaining()).append(':').append(Long.toHexString(lv.get(0)));
                iv.put(0, 0x11223344);
                sb.append(",after=").append(Integer.toHexString(s.getInt(2)));
                sb.append(",ro=").append(s.asReadOnlyBuffer().asFloatBuffer().isReadOnly());
                System.out.println(sb);
            }
        }
        ByteBuffer o = ByteBuffer.allocate(10);
        System.out.println("bbviews-order " + o.order() + "," + o.duplicate().order() + "," + o.order(ByteOrder.LITTLE_ENDIAN).slice().order()
                + "," + o.asIntBuffer().order() + "," + ByteBuffer.allocateDirect(8).order(ByteOrder.LITTLE_ENDIAN).asLongBuffer().order()
                + "," + o.asReadOnlyBuffer().order());
    }

    static void bbro() {
        ByteBuffer r = ByteBuffer.wrap(new byte[]{1, 2, 3, 4}).asReadOnlyBuffer();
        System.out.println("bbro " + f(() -> r.hasArray()) + "," + f(() -> r.array()) + "," + f(() -> r.arrayOffset())
                + "," + f(() -> r.put((byte) 1)) + "," + f(() -> r.putInt(0, 1)) + "," + f(() -> r.compact()) + "," + r.isReadOnly()
                + "," + f(() -> r.get(1)) + "," + f(() -> r.duplicate().isReadOnly()) + "," + f(() -> r.slice().isReadOnly()));
        ByteBuffer rd = ByteBuffer.allocateDirect(4).asReadOnlyBuffer();
        System.out.println("bbro-direct " + rd.getClass().getName() + "," + f(() -> rd.put(0, (byte) 1)) + "," + f(() -> rd.asIntBuffer().put(0, 1))
                + "," + rd.isDirect() + "," + f(() -> rd.hasArray()));
        CharBuffer cs = CharBuffer.wrap("hello");
        System.out.println("cb-ro " + cs.isReadOnly() + "," + f(() -> cs.put('x')) + "," + f(() -> cs.hasArray()) + "," + cs.getClass().getName()
                + "," + f(() -> cs.subSequence(1, 3)) + "," + f(() -> cs.subSequence(3, 1)) + "," + f(() -> cs.get(7)));
    }

    static void bbbulk() {
        ByteBuffer b = ByteBuffer.allocate(8);
        System.out.println("bbbulk " + f(() -> b.put(new byte[9])) + "," + f(() -> b.get(new byte[4], 2, 3).position()) + ","
                + f(() -> b.get(new byte[4], 3, 3)) + "," + f(() -> b.put(0, new byte[4], 0, 4).position()) + "," + f(() -> b.put(6, new byte[4]))
                + "," + f(() -> b.get(-1)) + "," + f(() -> b.getInt(5)) + "," + f(() -> b.position(9)) + "," + f(() -> b.limit(-1)));
        ByteBuffer src = ByteBuffer.wrap("0123456789".getBytes());
        ByteBuffer dst = ByteBuffer.allocateDirect(6);
        System.out.println("bbbulk2 " + f(() -> dst.put(src.duplicate())) + "," + f(() -> dst.clear().put(src.duplicate().limit(6)).position())
                + "," + f(() -> dst.put(dst)) + "," + f(() -> dst.clear().put(2, src, 3, 4).get(5)) + "," + f(() -> dst.put(3, src, 0, 4)));
        ByteBuffer c = ByteBuffer.wrap("abcdef".getBytes());
        c.position(2);
        c.compact();
        System.out.println("bbbulk3 " + c.position() + "," + c.limit() + "," + new String(c.array(), 0, 4) + "," + c.mark().reset().position()
                + "," + f(() -> ByteBuffer.allocate(4).reset()) + "," + ByteBuffer.allocate(4).toString());
        IntBuffer ib = IntBuffer.allocate(4);
        System.out.println("ibbulk " + f(() -> ib.put(new int[]{1, 2, 3, 4, 5})) + "," + f(() -> ib.put(new int[]{1, 2}).get(new int[3])) + "," + ib
                + "," + f(() -> ib.put(1, IntBuffer.wrap(new int[]{9, 8}), 0, 2).get(2)));
    }

    static void bbslice() {
        ByteBuffer b = ByteBuffer.allocateDirect(16);
        for (int i = 0; i < 16; i++) b.put((byte) i);
        b.position(3).limit(11);
        ByteBuffer s = b.slice();
        ByteBuffer s2 = s.slice(2, 4);
        System.out.println("bbslice " + s.capacity() + "," + s.get(0) + "," + s2.get(0) + "," + s2.capacity() + "," + f(() -> s.slice(5, 4))
                + "," + s2.isDirect() + "," + s2.order() + "," + f(() -> s2.alignmentOffset(0, 4) >= 0) + "," + f(() -> ByteBuffer.allocate(8).alignedSlice(4).capacity())
                + "," + f(() -> ByteBuffer.wrap(new byte[8], 2, 4)) + "," + f(() -> ByteBuffer.wrap(new byte[8], 5, 4)));
        ByteBuffer h = ByteBuffer.wrap(new byte[20], 4, 10);
        System.out.println("bbslice-heap " + h.position() + "," + h.limit() + "," + h.arrayOffset() + "," + h.slice().arrayOffset() + "," + h.slice(1, 2).arrayOffset()
                + "," + h.duplicate().arrayOffset() + "," + f(() -> h.slice().alignmentOffset(1, 8)));
    }

    static void cbfacts() throws Exception {
        CharBuffer h = CharBuffer.allocate(10);
        h.put("hello").flip();
        CharBuffer ss = CharBuffer.wrap("abcdefgh", 2, 6);
        CharBuffer sb = CharBuffer.wrap(new StringBuilder("xyz123"));
        System.out.println("cbfacts " + h + "," + h.length() + "," + h.charAt(1) + "," + f(() -> h.charAt(5)) + "," + ss + "," + ss.position() + "," + ss.subSequence(1, 3)
                + "," + sb.chars().sum() + "," + h.codePoints().count() + "," + h.subSequence(1, 4).position() + "," + h.toString().length()
                + "," + f(() -> CharBuffer.wrap("ab").append('c')) + "," + h.duplicate().append("!").position() + "," + CharBuffer.allocate(3).append("abc", 1, 3).flip());
        CharBuffer d = ByteBuffer.allocateDirect(8).asCharBuffer();
        d.put("wxyz").flip();
        System.out.println("cbdirect " + d + "," + d.isDirect() + "," + d.subSequence(1, 3) + "," + d.order() + "," + f(() -> d.array()) + "," + d.toString().indexOf("yz")
                + "," + f(() -> d.isEmpty()) + "," + d.compareTo(CharBuffer.wrap("wxy")));
        StringBuilder target = new StringBuilder();
        try { new StringReader("readme!").read(CharBuffer.wrap(new char[4])); } catch (IOException e) { }
        CharBuffer rb = CharBuffer.allocate(4);
        int n = f2(() -> new StringReader("readme!").read(rb));
        System.out.println("cbread " + n + "," + rb.flip() + "," + CharBuffer.wrap("ab").read(CharBuffer.allocate(1)));
    }
    interface IFact { int get() throws Exception; }
    static int f2(IFact x) { try { return x.get(); } catch (Exception e) { return -99; } }

    static void charset() throws Exception {
        String s = "aé€😀z\u0000";
        for (String cs : new String[]{"UTF-8", "UTF-16", "UTF-16LE", "UTF-16BE", "ISO-8859-1", "US-ASCII", "windows-1252", "UTF-32", "Shift_JIS", "GB18030", "x-UTF-16LE-BOM", "IBM437"}) {
            Charset c = Charset.forName(cs);
            byte[] e = s.getBytes(c);
            String back = new String(e, c);
            ByteBuffer enc = c.encode(s);
            CharBuffer dec = c.decode(ByteBuffer.wrap(e));
            System.out.println("charset " + cs + "," + c.name() + "," + e.length + "," + Arrays.hashCode(e) + "," + back.equals(s) + "," + back.length()
                    + "," + enc.remaining() + "," + dec.remaining() + "," + c.canEncode() + "," + c.newEncoder().maxBytesPerChar() + "," + c.newDecoder().averageCharsPerByte()
                    + "," + c.newEncoder().canEncode('€') + "," + c.aliases().size());
        }
        byte[] bad = {(byte) 0xC3, (byte) 0x28, (byte) 0xE2, (byte) 0x82, 'a', (byte) 0xF0, (byte) 0x9F, (byte) 0x98, (byte) 0xED, (byte) 0xA0, (byte) 0x80, (byte) 0xFF};
        String r = new String(bad, StandardCharsets.UTF_8);
        StringBuilder cps = new StringBuilder();
        r.codePoints().forEach(cp -> cps.append(Integer.toHexString(cp)).append(' '));
        System.out.println("charset-bad " + cps + "|" + StandardCharsets.UTF_8.decode(ByteBuffer.wrap(bad)).toString().equals(r));
        System.out.println("charset-lone " + Arrays.toString("x\uD800y".getBytes(StandardCharsets.UTF_8)) + "," + Arrays.toString("x\uDC00".getBytes(StandardCharsets.ISO_8859_1))
                + "," + Arrays.toString("é".getBytes("US-ASCII")) + "," + f(() -> Charset.forName("nope")) + "," + f(() -> Charset.forName("bad name!"))
                + "," + Charset.isSupported("utf8") + "," + Charset.forName("latin1").name() + "," + f(() -> Charset.defaultCharset().name()));
    }

    static void coder() throws Exception {
        CharsetDecoder d = StandardCharsets.UTF_8.newDecoder();
        System.out.println("coder-report " + f(() -> d.decode(ByteBuffer.wrap(new byte[]{'a', (byte) 0xC3, (byte) 0x28}))) + ","
                + f(() -> StandardCharsets.US_ASCII.newEncoder().encode(CharBuffer.wrap("aé"))) + ","
                + f(() -> StandardCharsets.UTF_8.newEncoder().encode(CharBuffer.wrap("a\uD800b"))));
        CharsetDecoder sd = StandardCharsets.UTF_8.newDecoder();
        CharBuffer out = CharBuffer.allocate(16);
        byte[] smile = "😀é".getBytes(StandardCharsets.UTF_8);
        StringBuilder res = new StringBuilder();
        ByteBuffer in = ByteBuffer.allocate(16);
        for (byte b : smile) {
            in.put(b).flip();
            CoderResult cr = sd.decode(in, out, false);
            res.append(cr.isUnderflow() ? 'U' : cr.isOverflow() ? 'O' : 'E').append(in.remaining());
            in.compact();
        }
        in.flip();
        res.append(sd.decode(in, out, true)).append(sd.flush(out));
        System.out.println("coder-split " + res + "," + out.flip());
        CharsetEncoder e = StandardCharsets.UTF_8.newEncoder();
        ByteBuffer small = ByteBuffer.allocate(3);
        CoderResult cr = e.encode(CharBuffer.wrap("ab€"), small, true);
        System.out.println("coder-ovf " + cr + "," + small.position() + "," + f(() -> cr.length()) + "," + f(() -> CoderResult.UNDERFLOW.length())
                + "," + CoderResult.malformedForLength(2) + "," + CoderResult.unmappableForLength(1).isUnmappable());
        CharsetEncoder le = StandardCharsets.ISO_8859_1.newEncoder().onUnmappableCharacter(CodingErrorAction.REPLACE).replaceWith(new byte[]{'?'});
        System.out.println("coder-repl " + new String(le.encode(CharBuffer.wrap("a€b")).array(), 0, 3) + "," + f(() -> le.replaceWith(new byte[0]))
                + "," + f(() -> StandardCharsets.UTF_8.newDecoder().replaceWith("")) + "," + f(() -> e.reset().encode(CharBuffer.wrap("x"), ByteBuffer.allocate(4), false).isUnderflow())
                + "," + f(() -> StandardCharsets.UTF_16.newEncoder().encode(CharBuffer.wrap("A")).remaining()));
        InputStreamReader isr = new InputStreamReader(new ByteArrayInputStream("héllo😀".getBytes(StandardCharsets.UTF_8)), StandardCharsets.UTF_8);
        char[] buf = new char[3];
        StringBuilder got = new StringBuilder();
        int n;
        while ((n = isr.read(buf)) > 0) got.append(n).append(':').append(new String(buf, 0, n)).append('|');
        System.out.println("coder-isr " + got + "," + isr.getEncoding());
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        OutputStreamWriter w = new OutputStreamWriter(bo, "UTF-16");
        w.write("ab");
        w.write('\uD83D');
        w.flush();
        int mid = bo.size();
        w.write('\uDE00');
        w.close();
        System.out.println("coder-osw " + mid + "," + bo.size() + "," + Arrays.toString(bo.toByteArray()) + "," + w.getEncoding() + "," + f(() -> { w.write('x'); return "ok"; }));
    }

    static void filechan() throws Exception {
        Path p = Files.createTempFile("nio19", ".bin");
        try (FileChannel fc = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            ByteBuffer d = ByteBuffer.allocateDirect(100);
            for (int i = 0; i < 100; i++) d.put((byte) i);
            d.flip();
            int w = fc.write(d);
            ByteBuffer h1 = ByteBuffer.wrap("HEAD".getBytes()), h2 = ByteBuffer.wrap("TAIL".getBytes());
            long gw = fc.write(new ByteBuffer[]{h1, h2}, 0, 2);
            int pw = fc.write(ByteBuffer.wrap("XY".getBytes()), 200);
            ByteBuffer r = ByteBuffer.allocate(10);
            int pr = fc.read(r, 50);
            ByteBuffer a = ByteBuffer.allocate(3), b = ByteBuffer.allocateDirect(5);
            fc.position(0);
            long sr = fc.read(new ByteBuffer[]{a, b});
            int eof = fc.read(ByteBuffer.allocate(4), 1000);
            System.out.println("filechan " + w + "," + gw + "," + pw + "," + fc.size() + "," + pr + "," + r.get(0) + "," + sr + "," + a.get(0) + "," + b.get(4) + "," + eof + "," + fc.position()
                    + "," + f(() -> fc.read(ByteBuffer.allocate(1), -1)) + "," + f(() -> fc.position(-1)) + "," + f(() -> fc.truncate(50).size()) + "," + fc.position()
                    + "," + f(() -> fc.read(ByteBuffer.wrap(new byte[2]).asReadOnlyBuffer())) + "," + f(() -> fc.write(ByteBuffer.allocate(0))));
            FileLock l = fc.lock(0, 10, false);
            System.out.println("filelock " + l.isValid() + "," + l.isShared() + "," + l.overlaps(5, 2) + "," + f(() -> fc.tryLock(0, 5, false)) + "," + f(() -> fc.lock(-1, 1, false)));
            l.release();
            fc.force(true);
        }
        try (FileChannel ro = FileChannel.open(p, StandardOpenOption.READ)) {
            System.out.println("filechan-ro " + f(() -> ro.write(ByteBuffer.allocate(1))) + "," + f(() -> ro.truncate(1)) + "," + f(() -> ro.lock()) + "," + ro.size());
        }
        FileChannel closed = FileChannel.open(p);
        closed.close();
        System.out.println("filechan-closed " + f(() -> closed.size()) + "," + f(() -> closed.read(ByteBuffer.allocate(1))) + "," + closed.isOpen());
        try (RandomAccessFile raf = new RandomAccessFile(p.toFile(), "rw")) {
            FileChannel c = raf.getChannel();
            raf.seek(10);
            System.out.println("raf-chan " + c.position() + "," + raf.readByte() + "," + c.position() + "," + (c == raf.getChannel()));
        }
        Files.delete(p);
    }

    static void transfer() throws Exception {
        Path a = Files.createTempFile("nio19a", ".bin"), b = Files.createTempFile("nio19b", ".bin");
        byte[] data = new byte[300_000];
        for (int i = 0; i < data.length; i++) data[i] = (byte) (i * 31);
        Files.write(a, data);
        try (FileChannel in = FileChannel.open(a); FileChannel out = FileChannel.open(b, StandardOpenOption.WRITE, StandardOpenOption.READ)) {
            long t1 = in.transferTo(1000, 200_000, out);
            long t2 = out.transferFrom(in, 250_000, 100_000);
            long t3 = out.transferFrom(in, 500_000, 10);
            long t4 = in.transferTo(299_990, 100, out);
            long t5 = in.transferTo(400_000, 100, out);
            in.position(5);
            long t6 = out.transferFrom(in, 0, 7);
            byte[] got = Files.readAllBytes(b);
            System.out.println("transfer " + t1 + "," + t2 + "," + t3 + "," + t4 + "," + t5 + "," + t6 + "," + in.position() + "," + out.position() + "," + out.size()
                    + "," + Arrays.hashCode(got) + "," + f(() -> in.transferTo(-1, 1, out)) + "," + f(() -> out.transferFrom(in, -1, 1)));
            Pipe pipe = Pipe.open();
            pipe.sink().configureBlocking(true);
            long tp = in.transferTo(0, 1000, pipe.sink());
            ByteBuffer pb = ByteBuffer.allocate(2000);
            int pr = pipe.source().read(pb);
            pipe.sink().close();
            long tf = out.transferFrom(pipe.source(), 0, 10);
            System.out.println("transfer-pipe " + tp + "," + pr + "," + pb.get(999) + "," + tf);
            pipe.source().close();
        }
        Files.delete(a);
        Files.delete(b);
    }

    static void mapped() throws Exception {
        Path p = Files.createTempFile("nio19m", ".bin");
        try (FileChannel fc = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            MappedByteBuffer m = fc.map(FileChannel.MapMode.READ_WRITE, 0, 8192);
            m.putLong(0, 0x0102030405060708L).putInt(4096, 77);
            m.force();
            MappedByteBuffer r = fc.map(FileChannel.MapMode.READ_ONLY, 4096, 4);
            MappedByteBuffer pv = fc.map(FileChannel.MapMode.PRIVATE, 0, 8);
            pv.put(0, (byte) 9);
            System.out.println("mapped " + m.isDirect() + "," + m.getClass().getName() + "," + fc.size() + "," + r.getInt(0) + "," + r.isReadOnly() + "," + f(() -> r.put((byte) 1))
                    + "," + pv.get(0) + "," + m.get(0) + "," + m.slice(4096, 8).getInt(0) + "," + f(() -> fc.map(FileChannel.MapMode.READ_WRITE, -1, 1))
                    + "," + f(() -> fc.map(FileChannel.MapMode.READ_ONLY, 0, Integer.MAX_VALUE + 1L)) + "," + m.force(0, 8).position() + "," + f(() -> m.force(8190, 4)) + ","
                    + m.load().capacity() + "," + m.duplicate().getClass().getName() + "," + m.asReadOnlyBuffer().getClass().getName());
        }
        byte[] disk = Files.readAllBytes(p);
        System.out.println("mapped-disk " + disk.length + "," + disk[0] + "," + disk[7] + "," + disk[4099]);
        Files.delete(p);
    }

    static void sockchan() throws Exception {
        try (ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            SocketChannel c = SocketChannel.open(ss.getLocalAddress());
            SocketChannel s = ss.accept();
            ByteBuffer d = ByteBuffer.allocateDirect(64);
            d.put("hello-direct".getBytes()).flip();
            int w1 = c.write(d);
            long w2 = c.write(new ByteBuffer[]{ByteBuffer.wrap("A".getBytes()), ByteBuffer.wrap("BC".getBytes()).asReadOnlyBuffer()});
            c.shutdownOutput();
            ByteBuffer in = ByteBuffer.allocate(5);
            ByteBuffer in2 = ByteBuffer.allocateDirect(100);
            long rr = 0, n;
            while ((n = s.read(new ByteBuffer[]{in, in2})) >= 0) rr += n;
            in.flip();
            in2.flip();
            byte[] rest = new byte[in2.remaining()];
            in2.get(rest);
            System.out.println("sockchan " + w1 + "," + w2 + "," + rr + "," + new String(in.array(), 0, 5) + new String(rest) + "," + s.read(ByteBuffer.allocate(1))
                    + "," + c.isConnected() + "," + f(() -> c.write(ByteBuffer.allocate(1))) + "," + c.socket().isOutputShutdown() + "," + s.getOption(java.net.StandardSocketOptions.TCP_NODELAY)
                    + "," + f(() -> c.getOption(java.net.StandardSocketOptions.SO_LINGER)) + "," + f(() -> s.setOption(java.net.StandardSocketOptions.SO_RCVBUF, 4096) != null)
                    + "," + f(() -> c.read(ByteBuffer.allocate(1).asReadOnlyBuffer())));
            s.close();
            c.close();
            System.out.println("sockchan-closed " + f(() -> c.read(ByteBuffer.allocate(1))) + "," + f(() -> c.getRemoteAddress()) + "," + f(() -> ss.socket().getReuseAddress() || true));
            SocketChannel nb = SocketChannel.open();
            nb.configureBlocking(false);
            boolean imm = nb.connect(ss.getLocalAddress());
            SocketChannel acc = null;
            while (acc == null) acc = ss.accept();
            while (!nb.finishConnect()) Thread.onSpinWait();
            acc.configureBlocking(false);
            int nbread = acc.read(ByteBuffer.allocate(4));
            System.out.println("sockchan-nb " + nbread + "," + nb.isConnected() + "," + nb.isConnectionPending() + "," + f(() -> nb.connect(ss.getLocalAddress())) + "," + nb.isBlocking());
            nb.close();
            acc.close();
        }
        try (SocketChannel refused = SocketChannel.open()) {
            ServerSocket tmp = new ServerSocket(0);
            int port = tmp.getLocalPort();
            tmp.close();
            System.out.println("sockchan-refused " + f(() -> refused.connect(new InetSocketAddress(InetAddress.getLoopbackAddress(), port))).replaceAll("\\(.*", ""));
        }
    }

    static void selector() throws Exception {
        try (Selector sel = Selector.open(); ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0)).configureBlocking(false);
            SelectionKey sk = ss.register(sel, SelectionKey.OP_ACCEPT, "srv");
            int z = sel.selectNow();
            SocketChannel c = SocketChannel.open();
            c.configureBlocking(false);
            c.connect(ss.getLocalAddress());
            SelectionKey ck = c.register(sel, SelectionKey.OP_CONNECT);
            int events = 0;
            SocketChannel acc = null;
            StringBuilder seen = new StringBuilder();
            long dl = System.currentTimeMillis() + 10000;
            boolean wrote = false, read = false;
            while (!read && System.currentTimeMillis() < dl) {
                sel.select(200);
                Iterator<SelectionKey> it = sel.selectedKeys().iterator();
                while (it.hasNext()) {
                    SelectionKey k = it.next();
                    it.remove();
                    events++;
                    if (k.isAcceptable()) {
                        acc = ss.accept();
                        acc.configureBlocking(false);
                        acc.register(sel, SelectionKey.OP_READ, "acc");
                        seen.append('A');
                    } else if (k.isConnectable()) {
                        ((SocketChannel) k.channel()).finishConnect();
                        k.interestOps(SelectionKey.OP_WRITE);
                        seen.append('C');
                    } else if (k.isWritable() && !wrote) {
                        ((SocketChannel) k.channel()).write(ByteBuffer.wrap("ping".getBytes()));
                        k.interestOps(0);
                        wrote = true;
                        seen.append('W');
                    } else if (k.isReadable()) {
                        ByteBuffer b = ByteBuffer.allocate(16);
                        int n = ((SocketChannel) k.channel()).read(b);
                        seen.append('R').append(n).append(k.attachment());
                        read = true;
                    }
                }
            }
            sel.wakeup();
            int afterWake = sel.select(5000);
            sk.cancel();
            int keys = sel.keys().size();
            sel.selectNow();
            System.out.println("selector " + z + "," + seen.toString().replaceAll("^(.*?)C", "C").length() + "," + read + "," + afterWake + "," + keys + "," + sel.keys().size()
                    + "," + sk.isValid() + "," + f(() -> sk.interestOps()) + "," + f(() -> ck.interestOps(-1)) + "," + f(() -> ss.register(sel, SelectionKey.OP_READ)));
            c.close();
            if (acc != null) acc.close();
        }
        Selector s2 = Selector.open();
        s2.close();
        System.out.println("selector-closed " + f(() -> s2.select(1)) + "," + f(() -> s2.keys()) + "," + s2.isOpen());
    }

    static void dgram() throws Exception {
        try (DatagramChannel a = DatagramChannel.open(); DatagramChannel b = DatagramChannel.open()) {
            a.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            b.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            int sent = b.send(ByteBuffer.wrap("dgram!".getBytes()), a.getLocalAddress());
            ByteBuffer r = ByteBuffer.allocateDirect(3);
            SocketAddress from = a.receive(r);
            b.connect(a.getLocalAddress());
            a.connect(b.getLocalAddress());
            b.write(ByteBuffer.wrap("x2".getBytes()));
            ByteBuffer r2 = ByteBuffer.allocate(10);
            int n2 = a.read(r2);
            System.out.println("dgram " + sent + "," + r.position() + "," + from.equals(b.getLocalAddress()) + "," + n2 + "," + a.isConnected()
                    + "," + f(() -> a.send(ByteBuffer.allocate(1), new InetSocketAddress(InetAddress.getLoopbackAddress(), 9))));
            a.configureBlocking(false);
            System.out.println("dgram-nb " + a.receive(ByteBuffer.allocate(4)) + "," + a.read(ByteBuffer.allocate(4)));
        }
    }

    static void pipe() throws Exception {
        Pipe p = Pipe.open();
        p.source().configureBlocking(false);
        int empty = p.source().read(ByteBuffer.allocate(4));
        p.sink().write(ByteBuffer.wrap("pp".getBytes()));
        ByteBuffer b = ByteBuffer.allocate(4);
        int n = 0;
        long dl = System.currentTimeMillis() + 5000;
        while (n == 0 && System.currentTimeMillis() < dl) n = p.source().read(b);
        p.sink().close();
        int eof = -2;
        while ((eof = p.source().read(ByteBuffer.allocate(1))) == 0) Thread.onSpinWait();
        System.out.println("pipe " + empty + "," + n + "," + eof + "," + f(() -> p.sink().write(ByteBuffer.allocate(1))));
        p.source().close();
    }

    static void http() throws Exception {
        com.sun.net.httpserver.HttpServer srv = com.sun.net.httpserver.HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
        srv.createContext("/echo", ex -> {
            byte[] body = ex.getRequestBody().readAllBytes();
            String resp = ex.getRequestMethod() + ":" + body.length + ":" + ex.getRequestHeaders().getFirst("X-T") + ":" + ex.getRequestURI().getQuery();
            ex.getResponseHeaders().add("X-R", "r1");
            byte[] rb = resp.getBytes(StandardCharsets.UTF_8);
            ex.sendResponseHeaders(201, rb.length);
            ex.getResponseBody().write(rb);
            ex.close();
        });
        srv.createContext("/big", ex -> {
            ex.sendResponseHeaders(200, 0);
            OutputStream o = ex.getResponseBody();
            byte[] chunk = new byte[8192];
            for (int i = 0; i < chunk.length; i++) chunk[i] = (byte) ('a' + i % 26);
            for (int i = 0; i < 64; i++) o.write(chunk);
            o.close();
        });
        srv.createContext("/redir", ex -> { ex.getResponseHeaders().add("Location", "/echo?r=1"); ex.sendResponseHeaders(302, -1); ex.close(); });
        srv.createContext("/gone", ex -> { ex.sendResponseHeaders(404, -1); ex.close(); });
        ExecutorService ex = Executors.newFixedThreadPool(2);
        srv.setExecutor(ex);
        srv.start();
        int port = srv.getAddress().getPort();
        String base = "http://127.0.0.1:" + port;
        try {
            java.net.http.HttpClient hc = java.net.http.HttpClient.newBuilder().followRedirects(java.net.http.HttpClient.Redirect.NORMAL).build();
            var r1 = hc.send(java.net.http.HttpRequest.newBuilder(URI.create(base + "/echo?q=1")).header("X-T", "tv")
                    .POST(java.net.http.HttpRequest.BodyPublishers.ofString("héllo")).build(), java.net.http.HttpResponse.BodyHandlers.ofString());
            var r2 = hc.send(java.net.http.HttpRequest.newBuilder(URI.create(base + "/big")).build(), java.net.http.HttpResponse.BodyHandlers.ofByteArray());
            var r3 = hc.send(java.net.http.HttpRequest.newBuilder(URI.create(base + "/redir")).build(), java.net.http.HttpResponse.BodyHandlers.ofString());
            var r4 = hc.sendAsync(java.net.http.HttpRequest.newBuilder(URI.create(base + "/gone")).build(), java.net.http.HttpResponse.BodyHandlers.discarding()).get(20, TimeUnit.SECONDS);
            byte[] big = r2.body();
            System.out.println("httpclient " + r1.statusCode() + "," + r1.body() + "," + r1.headers().firstValue("x-r").orElse("-") + "," + r1.version() + "," + r2.statusCode() + "," + big.length
                    + "," + Arrays.hashCode(big) + "," + r3.statusCode() + "," + r3.body() + "," + r4.statusCode());
            List<CompletableFuture<Integer>> fs = new ArrayList<>();
            for (int i = 0; i < 8; i++)
                fs.add(hc.sendAsync(java.net.http.HttpRequest.newBuilder(URI.create(base + "/echo?i=" + i)).build(), java.net.http.HttpResponse.BodyHandlers.ofString()).thenApply(r -> r.body().length()));
            int sum = 0;
            for (var fu : fs) sum += fu.get(20, TimeUnit.SECONDS);
            System.out.println("httpclient-async " + sum);
            HttpURLConnection u = (HttpURLConnection) new URL(base + "/echo?u=2").openConnection();
            u.setRequestMethod("PUT");
            u.setDoOutput(true);
            u.setRequestProperty("X-T", "uc");
            u.getOutputStream().write(new byte[10]);
            String ub = new String(u.getInputStream().readAllBytes());
            HttpURLConnection g = (HttpURLConnection) new URL(base + "/gone").openConnection();
            System.out.println("urlconn " + u.getResponseCode() + "," + ub + "," + u.getHeaderField("X-R") + "," + g.getResponseCode() + "," + f(() -> g.getInputStream()).replaceAll(":.*", "")
                    + "," + f(() -> new URL("http://127.0.0.1:1/x").openConnection().getInputStream()).replaceAll(":.*", ""));
        } finally {
            srv.stop(0);
            ex.shutdownNow();
        }
    }

    static javax.net.ssl.SSLContext sslContext() throws Exception {
        String ks = System.getProperty("nio19.ks");
        java.security.KeyStore k = java.security.KeyStore.getInstance("PKCS12");
        try (InputStream in = new FileInputStream(ks)) { k.load(in, "changeit".toCharArray()); }
        javax.net.ssl.KeyManagerFactory kmf = javax.net.ssl.KeyManagerFactory.getInstance("SunX509");
        kmf.init(k, "changeit".toCharArray());
        javax.net.ssl.TrustManagerFactory tmf = javax.net.ssl.TrustManagerFactory.getInstance("PKIX");
        tmf.init(k);
        javax.net.ssl.SSLContext c = javax.net.ssl.SSLContext.getInstance("TLS");
        c.init(kmf.getKeyManagers(), tmf.getTrustManagers(), null);
        return c;
    }

    static void ssl() throws Exception {
        if (System.getProperty("nio19.ks") == null) { System.out.println("ssl skipped"); return; }
        javax.net.ssl.SSLContext ctx = sslContext();
        for (String proto : new String[]{"TLSv1.3", "TLSv1.2"}) {
            javax.net.ssl.SSLEngine cl = ctx.createSSLEngine("localhost", 443), sv = ctx.createSSLEngine();
            cl.setUseClientMode(true);
            sv.setUseClientMode(false);
            cl.setEnabledProtocols(new String[]{proto});
            sv.setEnabledProtocols(new String[]{proto});
            int pk = cl.getSession().getPacketBufferSize(), ap = cl.getSession().getApplicationBufferSize();
            ByteBuffer c2s = ByteBuffer.allocateDirect(pk), s2c = ByteBuffer.allocate(pk);
            ByteBuffer cApp = ByteBuffer.wrap(("hello-" + proto).getBytes()), sApp = ByteBuffer.wrap("world".getBytes());
            ByteBuffer cIn = ByteBuffer.allocate(ap), sIn = ByteBuffer.allocateDirect(ap);
            int loops = 0;
            while (loops++ < 200) {
                cl.wrap(cApp, c2s);
                sv.wrap(sApp, s2c);
                c2s.flip();
                s2c.flip();
                cl.unwrap(s2c, cIn);
                sv.unwrap(c2s, sIn);
                c2s.compact();
                s2c.compact();
                runTasks(cl);
                runTasks(sv);
                if (!cApp.hasRemaining() && !sApp.hasRemaining() && cIn.position() == 5 && sIn.position() == cApp.limit()) break;
            }
            sIn.flip();
            byte[] got = new byte[sIn.remaining()];
            sIn.get(got);
            System.out.println("sslengine " + proto + "," + cl.getSession().getProtocol() + "," + (cl.getSession().getCipherSuite().length() > 0) + "," + new String(got) + ","
                    + new String(cIn.array(), 0, cIn.position()) + "," + cl.getHandshakeStatus() + "," + (loops < 200) + "," + sv.getSession().getPeerHost());
            cl.closeOutbound();
            ByteBuffer cl2 = ByteBuffer.allocate(pk);
            javax.net.ssl.SSLEngineResult res = cl.wrap(ByteBuffer.allocate(0), cl2);
            cl2.flip();
            javax.net.ssl.SSLEngineResult res2 = sv.unwrap(cl2, ByteBuffer.allocate(ap));
            System.out.println("sslengine-close " + res.getStatus() + "," + res2.getStatus() + "," + sv.isInboundDone() + "," + cl.isOutboundDone()
                    + "," + f(() -> sv.unwrap(ByteBuffer.wrap(new byte[]{22, 3, 3, 0, 1, 0}), ByteBuffer.allocate(ap)).getStatus()));
        }
        javax.net.ssl.SSLEngine bad = ctx.createSSLEngine();
        bad.setUseClientMode(false);
        System.out.println("sslengine-garbage " + f(() -> bad.unwrap(ByteBuffer.wrap("GET / HTTP/1.1\r\n\r\n".getBytes()), ByteBuffer.allocate(20000)).getStatus()).replaceAll(":.*", ""));
    }

    static void runTasks(javax.net.ssl.SSLEngine e) {
        Runnable r;
        while ((r = e.getDelegatedTask()) != null) r.run();
    }

    static void sslsock() throws Exception {
        if (System.getProperty("nio19.ks") == null) { System.out.println("sslsock skipped"); return; }
        javax.net.ssl.SSLContext ctx = sslContext();
        try (javax.net.ssl.SSLServerSocket ss = (javax.net.ssl.SSLServerSocket) ctx.getServerSocketFactory().createServerSocket(0, 5, InetAddress.getLoopbackAddress())) {
            CompletableFuture<String> srv = CompletableFuture.supplyAsync(() -> {
                try (Socket s = ss.accept()) {
                    DataInputStream in = new DataInputStream(s.getInputStream());
                    byte[] b = new byte[in.readInt()];
                    in.readFully(b);
                    s.getOutputStream().write(("ack" + b.length).getBytes());
                    s.getOutputStream().flush();
                    return ((javax.net.ssl.SSLSocket) s).getSession().getProtocol() + ":" + Arrays.hashCode(b);
                } catch (Exception e) { return "!" + e; }
            });
            try (javax.net.ssl.SSLSocket c = (javax.net.ssl.SSLSocket) ctx.getSocketFactory().createSocket("localhost", ss.getLocalPort())) {
                byte[] payload = new byte[100_000];
                for (int i = 0; i < payload.length; i++) payload[i] = (byte) (i ^ (i >> 8));
                DataOutputStream o = new DataOutputStream(c.getOutputStream());
                o.writeInt(payload.length);
                o.write(payload);
                o.flush();
                byte[] ack = new byte[9];
                new DataInputStream(c.getInputStream()).readFully(ack);
                System.out.println("sslsock " + new String(ack) + "," + srv.get(30, TimeUnit.SECONDS) + "," + c.getSession().getPeerCertificates().length + "," + c.getApplicationProtocol());
            }
            // HTTPS through HttpClient with the same context
            com.sun.net.httpserver.HttpsServer hs = com.sun.net.httpserver.HttpsServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
            hs.setHttpsConfigurator(new com.sun.net.httpserver.HttpsConfigurator(ctx));
            hs.createContext("/s", ex -> { byte[] r = ("secure:" + ex.getRequestMethod()).getBytes(); ex.sendResponseHeaders(200, r.length); ex.getResponseBody().write(r); ex.close(); });
            ExecutorService es = Executors.newFixedThreadPool(2);
            hs.setExecutor(es);
            hs.start();
            try {
                java.net.http.HttpClient hc = java.net.http.HttpClient.newBuilder().sslContext(ctx).version(java.net.http.HttpClient.Version.HTTP_1_1).build();
                var r = hc.send(java.net.http.HttpRequest.newBuilder(URI.create("https://localhost:" + hs.getAddress().getPort() + "/s")).build(), java.net.http.HttpResponse.BodyHandlers.ofString());
                javax.net.ssl.HttpsURLConnection u = (javax.net.ssl.HttpsURLConnection) new URL("https://localhost:" + hs.getAddress().getPort() + "/s").openConnection();
                u.setSSLSocketFactory(ctx.getSocketFactory());
                System.out.println("https " + r.statusCode() + "," + r.body() + "," + r.sslSession().map(s -> s.getProtocol()).orElse("-") + "," + u.getResponseCode() + "," + new String(u.getInputStream().readAllBytes()));
            } finally {
                hs.stop(0);
                es.shutdownNow();
            }
        }
    }

    /** Minimal RFC 6455 server on a plain ServerSocket; client is java.net.http.WebSocket. */
    static void ws() throws Exception {
        try (ServerSocket ss = new ServerSocket(0, 5, InetAddress.getLoopbackAddress())) {
            Thread t = new Thread(() -> {
                try (Socket s = ss.accept()) {
                    BufferedReader r = new BufferedReader(new InputStreamReader(s.getInputStream(), StandardCharsets.ISO_8859_1));
                    String key = null, line;
                    while ((line = r.readLine()) != null && !line.isEmpty())
                        if (line.toLowerCase().startsWith("sec-websocket-key:")) key = line.substring(18).trim();
                    String acc = Base64.getEncoder().encodeToString(java.security.MessageDigest.getInstance("SHA-1")
                            .digest((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").getBytes(StandardCharsets.ISO_8859_1)));
                    OutputStream o = s.getOutputStream();
                    o.write(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + acc + "\r\n\r\n").getBytes(StandardCharsets.ISO_8859_1));
                    o.flush();
                    InputStream in = s.getInputStream();
                    // the BufferedReader may have consumed nothing past the header since the client waits for 101
                    while (true) {
                        int b0 = in.read(), b1 = in.read();
                        if (b0 < 0) return;
                        int op = b0 & 0xF;
                        long len = b1 & 0x7F;
                        if (len == 126) len = (in.read() << 8) | in.read();
                        else if (len == 127) { len = 0; for (int i = 0; i < 8; i++) len = (len << 8) | in.read(); }
                        byte[] mask = in.readNBytes(4);
                        byte[] p = in.readNBytes((int) len);
                        for (int i = 0; i < p.length; i++) p[i] ^= mask[i % 4];
                        // echo unmasked, same opcode
                        o.write(0x80 | op);
                        if (p.length < 126) o.write(p.length);
                        else if (p.length < 65536) { o.write(126); o.write(p.length >> 8); o.write(p.length & 0xFF); }
                        else { o.write(127); for (int i = 7; i >= 0; i--) o.write((int) ((long) p.length >> (8 * i)) & 0xFF); }
                        o.write(p);
                        o.flush();
                        if (op == 8) return;
                    }
                } catch (Exception e) {
                    System.out.println("ws-server " + ex(e));
                }
            });
            t.setDaemon(true);
            t.start();
            StringBuffer got = new StringBuffer();
            CompletableFuture<String> closed = new CompletableFuture<>();
            CountDownLatch msgs = new CountDownLatch(3);
            java.net.http.WebSocket ws = java.net.http.HttpClient.newHttpClient().newWebSocketBuilder()
                    .buildAsync(URI.create("ws://127.0.0.1:" + ss.getLocalPort() + "/ws"), new java.net.http.WebSocket.Listener() {
                        StringBuilder part = new StringBuilder();
                        public CompletionStage<?> onText(java.net.http.WebSocket w, CharSequence data, boolean last) {
                            part.append(data);
                            if (last) { got.append("T:").append(part.length() > 40 ? part.length() + "/" + part.toString().hashCode() : part).append('|'); part.setLength(0); msgs.countDown(); }
                            w.request(1);
                            return null;
                        }
                        ByteArrayOutputStream bin = new ByteArrayOutputStream();
                        public CompletionStage<?> onBinary(java.net.http.WebSocket w, ByteBuffer data, boolean last) {
                            byte[] b = new byte[data.remaining()];
                            data.get(b);
                            bin.write(b, 0, b.length);
                            if (last) { got.append("B:").append(bin.size()).append('/').append(Arrays.hashCode(bin.toByteArray())).append('|'); bin.reset(); msgs.countDown(); }
                            w.request(1);
                            return null;
                        }
                        public CompletionStage<?> onClose(java.net.http.WebSocket w, int code, String reason) { closed.complete(code + ":" + reason); return null; }
                        public void onError(java.net.http.WebSocket w, Throwable e) { closed.complete("err " + ex(e)); }
                    }).get(20, TimeUnit.SECONDS);
            ws.sendText("héllo-ws", true).get(10, TimeUnit.SECONDS);
            byte[] bin = new byte[70_000];
            for (int i = 0; i < bin.length; i++) bin[i] = (byte) (i * 13);
            ws.sendBinary(ByteBuffer.wrap(bin), true).get(10, TimeUnit.SECONDS);
            StringBuilder longText = new StringBuilder();
            for (int i = 0; i < 300; i++) longText.append("€").append(i);
            ws.sendText(longText, true).get(10, TimeUnit.SECONDS);
            boolean all = msgs.await(20, TimeUnit.SECONDS);
            ws.sendClose(1000, "bye").get(10, TimeUnit.SECONDS);
            String cl = closed.get(20, TimeUnit.SECONDS);
            System.out.println("ws " + all + "," + got + "," + cl + "," + ws.isOutputClosed() + "," + f(() -> ws.sendText("x", true).get()).replaceAll(":.*", ""));
        }
    }
}
