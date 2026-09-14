import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;
import org.apache.lucene.store.ByteBuffersDataInput;

/** Unit-level differential probe for org.apache.lucene.store.ByteBuffersDataInput. */
public class BBDinProbe {
    static void p(String label, Object v) {
        System.out.println(label + "=" + v);
        System.out.flush();
    }

    interface T<R> {
        R get() throws Exception;
    }

    static <R> void check(String label, T<R> t) {
        try {
            p(label, t.get());
        } catch (Throwable e) {
            p(label, "THREW " + e.getClass().getName()
                + (e.getMessage() == null ? "" : ": " + e.getMessage()));
        }
    }

    static byte[] ramp(int n, int start) {
        byte[] b = new byte[n];
        for (int i = 0; i < n; i++) {
            b[i] = (byte) (start + i);
        }
        return b;
    }

    /** One block of `n` bytes. */
    static ByteBuffersDataInput single(int n) {
        return new ByteBuffersDataInput(List.of(ByteBuffer.wrap(ramp(n, 0))));
    }

    /** `count` equal power-of-two blocks of `blockSize`, values continuous across blocks. */
    static ByteBuffersDataInput multi(int blockSize, int count, int lastSize) {
        List<ByteBuffer> bufs = new ArrayList<>();
        int v = 0;
        for (int i = 0; i < count - 1; i++) {
            bufs.add(ByteBuffer.wrap(ramp(blockSize, v)));
            v += blockSize;
        }
        bufs.add(ByteBuffer.wrap(ramp(lastSize, v)));
        return new ByteBuffersDataInput(bufs);
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) {
            sb.append(String.format("%02x", x));
        }
        return sb.toString();
    }

    public static void main(String[] a) throws Exception {
        // ---- single block ----
        check("s.size", () -> single(64).size());
        check("s.position0", () -> single(64).position());
        check("s.readByte", () -> single(64).readByte());
        check("s.readByteX3", () -> {
            ByteBuffersDataInput in = single(64);
            return in.readByte() + "," + in.readByte() + "," + in.readByte();
        });
        check("s.readShort", () -> single(64).readShort());
        check("s.readInt", () -> single(64).readInt());
        check("s.readLong", () -> single(64).readLong());
        check("s.readByteAt10", () -> single(64).readByte(10L));
        check("s.readIntAt10", () -> single(64).readInt(10L));
        check("s.readBytes8", () -> {
            byte[] d = new byte[8];
            single(64).readBytes(d, 0, 8);
            return hex(d);
        });
        check("s.readBytesAll64", () -> {
            byte[] d = new byte[64];
            single(64).readBytes(d, 0, 64);
            return hex(d).substring(0, 16) + "..." + hex(d).substring(112);
        });
        check("s.seekThenPosition", () -> {
            ByteBuffersDataInput in = single(64);
            in.seek(20);
            return in.position() + ":" + in.readByte();
        });
        check("s.skipBytes", () -> {
            ByteBuffersDataInput in = single(64);
            in.skipBytes(5);
            return in.position() + ":" + in.readByte();
        });
        check("s.slice", () -> {
            ByteBuffersDataInput sl = single(64).slice(16, 32);
            return sl.size() + ":" + sl.readByte();
        });
        check("s.sliceReadBytes", () -> {
            ByteBuffersDataInput sl = single(64).slice(16, 32);
            byte[] d = new byte[8];
            sl.readBytes(d, 0, 8);
            return hex(d);
        });
        check("s.readPastEnd", () -> {
            ByteBuffersDataInput in = single(8);
            byte[] d = new byte[16];
            in.readBytes(d, 0, 16);
            return "NO THROW";
        });

        // ---- multiple blocks (16-byte pages, 4 blocks, last full) ----
        check("m.size", () -> multi(16, 4, 16).size());
        check("m.readByte", () -> multi(16, 4, 16).readByte());
        check("m.readBytesWithinBlock", () -> {
            byte[] d = new byte[8];
            multi(16, 4, 16).readBytes(d, 0, 8);
            return hex(d);
        });
        // The interesting one: a copy that starts in block 0 and ends in block 1.
        check("m.readBytesAcrossBlockBoundary", () -> {
            byte[] d = new byte[24];
            multi(16, 4, 16).readBytes(d, 0, 24);
            return hex(d);
        });
        check("m.readBytesWholeThing", () -> {
            byte[] d = new byte[64];
            multi(16, 4, 16).readBytes(d, 0, 64);
            return hex(d).substring(0, 8) + "..." + hex(d).substring(120);
        });
        check("m.readLongAcrossBoundary", () -> {
            ByteBuffersDataInput in = multi(16, 4, 16);
            in.seek(12);
            return in.readLong();
        });
        check("m.readIntAtAcrossBoundary", () -> multi(16, 4, 16).readInt(14L));
        check("m.sliceAcrossBlocks", () -> {
            ByteBuffersDataInput sl = multi(16, 4, 16).slice(8, 40);
            byte[] d = new byte[40];
            sl.readBytes(d, 0, 40);
            return hex(d).substring(0, 8) + "..." + hex(d).substring(72);
        });
        check("m.shortLastBlock", () -> {
            ByteBuffersDataInput in = multi(16, 4, 8);
            byte[] d = new byte[56];
            in.readBytes(d, 0, 56);
            return in.size() + ":" + hex(d).substring(104);
        });

        p("DONE", "ok");
    }
}
