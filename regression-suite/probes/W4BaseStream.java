import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;

/**
 * `H11-1` N1 / `H11-3` N3 — the ABSTRACT-CLASS superclass-walk hazard, made
 * falsifiable.
 *
 * CratonVM registers `java.io.InputStream.{read([B),read([BII),available,skip,
 * readAllBytes,readNBytes}` and `java.io.OutputStream.{write,flush,close}` as
 * BASE-CLASS FALLBACKS whose bodies are `ByteArrayInputStream` /
 * `ByteArrayOutputStream` implementations. Native dispatch keys on the
 * receiver's runtime class, and the one fallback walk follows `superclass`
 * links — so a small application subclass that declares only the abstract
 * primitive (`read()` / `write(int)`) and inherits everything else lands on
 * those bodies, against an object that has none of the state they read.
 *
 * Every case below is a subclass whose CORRECT answer is fixed by
 * `java.io.InputStream`'s own javadoc, so the oracle and the VM must agree
 * exactly. One line per case; diff the two runs on stdout.
 */
public class W4BaseStream {

    static void ck(String tag, Object got) {
        System.out.println("CK " + tag + " " + got);
    }

    static <T> void ckThrows(String tag, Callable c) {
        try {
            Object v = c.call();
            System.out.println("CK " + tag + " no-throw:" + v);
        } catch (Throwable t) {
            System.out.println("CK " + tag + " threw:" + t.getClass().getName());
        }
    }

    interface Callable { Object call() throws Exception; }

    /** The smallest legal InputStream: only the abstract primitive. */
    static final class Counting extends InputStream {
        private final byte[] data;
        private int pos;
        int readCalls;
        Counting(byte[] data) { this.data = data; }
        @Override public int read() {
            readCalls++;
            return pos < data.length ? (data[pos++] & 0xff) : -1;
        }
    }

    /** The smallest legal OutputStream: only the abstract primitive. */
    static final class Sink extends OutputStream {
        final ArrayList<Integer> seen = new ArrayList<>();
        boolean flushed, closed;
        @Override public void write(int b) { seen.add(b & 0xff); }
        @Override public void flush() { flushed = true; }
        @Override public void close() { closed = true; }
    }

    /** An OutputStream that does NOT override flush/close. */
    static final class BareSink extends OutputStream {
        int count;
        @Override public void write(int b) { count++; }
    }

    /** A FilterOutputStream subclass, the shape `native_filteros_close` serves. */
    static final class CountingFilter extends FilterOutputStream {
        CountingFilter(OutputStream o) { super(o); }
    }

    public static void main(String[] args) throws Exception {
        // --- InputStream.read(byte[]) on a subclass that only has read() ---
        Counting c1 = new Counting("hello".getBytes(StandardCharsets.UTF_8));
        byte[] buf = new byte[8];
        int n = c1.read(buf);
        ck("is.read(byte[]).n", n);
        ck("is.read(byte[]).content", new String(buf, 0, Math.max(n, 0), StandardCharsets.UTF_8));
        ck("is.read(byte[]).readCalls>0", c1.readCalls > 0);

        // --- InputStream.read(byte[],int,int) ---
        Counting c2 = new Counting("world!".getBytes(StandardCharsets.UTF_8));
        byte[] b2 = new byte[8];
        int n2 = c2.read(b2, 2, 3);
        ck("is.read(byte[],2,3).n", n2);
        ck("is.read(byte[],2,3).content", Arrays.toString(b2));

        // --- InputStream.readAllBytes() ---
        Counting c3 = new Counting("abcdefgh".getBytes(StandardCharsets.UTF_8));
        ck("is.readAllBytes", new String(c3.readAllBytes(), StandardCharsets.UTF_8));

        // --- InputStream.readNBytes(int) ---
        Counting c4 = new Counting("abcdefgh".getBytes(StandardCharsets.UTF_8));
        ck("is.readNBytes(3)", new String(c4.readNBytes(3), StandardCharsets.UTF_8));

        // --- InputStream.readNBytes(byte[],int,int) ---
        Counting c5 = new Counting("abcdefgh".getBytes(StandardCharsets.UTF_8));
        byte[] b5 = new byte[8];
        ck("is.readNBytes(buf,1,4).n", c5.readNBytes(b5, 1, 4));
        ck("is.readNBytes(buf,1,4).content", Arrays.toString(b5));

        // --- InputStream.skip(long) ---
        Counting c6 = new Counting("abcdefgh".getBytes(StandardCharsets.UTF_8));
        ck("is.skip(3)", c6.skip(3));
        ck("is.skip(3).then.read", c6.read());

        // --- InputStream.available() — the base default is 0 ---
        Counting c7 = new Counting("abc".getBytes(StandardCharsets.UTF_8));
        ck("is.available", c7.available());

        // --- InputStream.transferTo(OutputStream) ---
        Counting c8 = new Counting("transfer".getBytes(StandardCharsets.UTF_8));
        ByteArrayOutputStream sinkOut = new ByteArrayOutputStream();
        ck("is.transferTo.n", c8.transferTo(sinkOut));
        ck("is.transferTo.content", sinkOut.toString("UTF-8"));

        // --- OutputStream.write(byte[]) / write(byte[],int,int) on a subclass ---
        Sink s1 = new Sink();
        s1.write("hey".getBytes(StandardCharsets.UTF_8));
        ck("os.write(byte[]).seen", s1.seen);
        Sink s2 = new Sink();
        s2.write("abcdef".getBytes(StandardCharsets.UTF_8), 1, 3);
        ck("os.write(byte[],1,3).seen", s2.seen);

        // --- OutputStream.flush()/close() defaults on a subclass with neither ---
        BareSink b6 = new BareSink();
        b6.write("xy".getBytes(StandardCharsets.UTF_8));
        ckThrows("os.bare.flush", () -> { b6.flush(); return "ok"; });
        ckThrows("os.bare.close", () -> { b6.close(); return "ok"; });
        ck("os.bare.count", b6.count);

        // --- FilterOutputStream.close() must flush AND close the wrapped stream ---
        Sink inner = new Sink();
        CountingFilter f = new CountingFilter(inner);
        f.write("z".getBytes(StandardCharsets.UTF_8));
        f.close();
        ck("filteros.close.innerClosed", inner.closed);
        ck("filteros.close.innerSeen", inner.seen);

        // --- BufferedOutputStream over a user sink ---
        Sink inner2 = new Sink();
        BufferedOutputStream bo = new BufferedOutputStream(inner2);
        bo.write('A');
        bo.write("BC".getBytes(StandardCharsets.UTF_8), 0, 2);
        bo.flush();
        ck("bufferedos.seen", inner2.seen);

        // --- DataInputStream over a user InputStream ---
        Counting c9 = new Counting(new byte[] {0, 0, 0, 7, 1, 'Q'});
        DataInputStream dis = new DataInputStream(c9);
        ck("dis.readInt", dis.readInt());
        ck("dis.readBoolean", dis.readBoolean());
        ck("dis.readUnsignedByte", dis.readUnsignedByte());

        // --- DataOutputStream over a user OutputStream ---
        Sink s3 = new Sink();
        DataOutputStream dos = new DataOutputStream(s3);
        dos.writeInt(0x01020304);
        dos.writeBoolean(true);
        dos.writeChar('Z');
        dos.write(0x7f);
        dos.write(new byte[] {1, 2, 3}, 1, 2);
        ck("dos.seen", s3.seen);

        System.out.println("PASS W4BaseStream");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
