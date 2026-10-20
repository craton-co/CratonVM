import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;

/**
 * `java.io.DataInputStream` (15 owned §1.4 shadow rows) and
 * `DataOutputStream` (14) — the third-largest group in the `java.io` census
 * (`WORKER-4-2` §1.1), and the one where a divergence is least likely to be
 * noticed by eye: the values are BYTES.
 *
 * Everything written is dumped as hex and everything read is dumped as its
 * exact value, so byte order, sign extension, the modified-UTF-8 encoding and
 * the EOF contract are all checked rather than assumed.
 */
public class W4Data {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    interface Thunk { Object call() throws Exception; }

    static void ckT(String tag, Thunk t) {
        try {
            ck(tag, t.call());
        } catch (Throwable e) {
            ck(tag, "threw:" + e.getClass().getName());
        }
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) sb.append(String.format("%02x", x));
        return sb.toString();
    }

    interface Write { void run(DataOutputStream d) throws Exception; }

    static byte[] out(String tag, Write w) {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        try (DataOutputStream d = new DataOutputStream(bo)) {
            w.run(d);
            d.flush();
            ck(tag, hex(bo.toByteArray()) + " size=" + d.size());
        } catch (Throwable t) {
            ck(tag, "threw:" + t.getClass().getName());
        }
        return bo.toByteArray();
    }

    static DataInputStream in(byte[] b) {
        return new DataInputStream(new ByteArrayInputStream(b));
    }

    public static void main(String[] args) throws Exception {
        // ---- DataOutputStream: exact bytes, big-endian ------------------
        out("dos.writeInt", d -> d.writeInt(0x01020304));
        out("dos.writeInt.neg", d -> d.writeInt(-1));
        out("dos.writeShort", d -> d.writeShort(0x0102));
        out("dos.writeShort.truncates", d -> d.writeShort(0x11223344));
        out("dos.writeChar", d -> d.writeChar('A'));
        out("dos.writeLong", d -> d.writeLong(Long.MIN_VALUE));
        out("dos.writeFloat", d -> d.writeFloat(1.0f));
        out("dos.writeFloat.nan", d -> d.writeFloat(Float.NaN));
        out("dos.writeDouble", d -> d.writeDouble(-2.5d));
        out("dos.writeBoolean", d -> { d.writeBoolean(true); d.writeBoolean(false); });
        out("dos.writeByte", d -> d.writeByte(-1));
        out("dos.write.int", d -> d.write(0x1ff));
        out("dos.write.bytes", d -> d.write(new byte[] {1, 2, 3}, 1, 2));
        out("dos.writeBytes", d -> d.writeBytes("ABÿ"));
        out("dos.writeChars", d -> d.writeChars("AB"));
        out("dos.writeUTF.ascii", d -> d.writeUTF("AB"));
        out("dos.writeUTF.empty", d -> d.writeUTF(""));
        // Modified UTF-8: U+0000 is TWO bytes, not one, and a supplementary
        // character is a surrogate PAIR of three bytes each.
        out("dos.writeUTF.nul", d -> d.writeUTF("a\0b"));
        out("dos.writeUTF.latin", d -> d.writeUTF("café"));
        out("dos.writeUTF.cjk", d -> d.writeUTF("日"));
        out("dos.writeUTF.supplementary", d -> d.writeUTF("😀"));
        out("dos.size.accumulates", d -> { d.writeInt(1); d.writeLong(2); d.writeByte(3); });

        // ---- DataInputStream: exact values ------------------------------
        byte[] ints = {0x01, 0x02, 0x03, 0x04, (byte) 0xff, (byte) 0xff, (byte) 0xff, (byte) 0xff};
        ckT("dis.readInt", () -> in(ints).readInt());
        ckT("dis.readInt.negative", () -> {
            DataInputStream d = in(ints);
            d.readInt();
            return d.readInt();
        });
        byte[] shorts = {(byte) 0xff, (byte) 0xfe, (byte) 0x80, 0x00};
        ckT("dis.readShort", () -> in(shorts).readShort());
        ckT("dis.readUnsignedShort", () -> in(shorts).readUnsignedShort());
        ckT("dis.readChar", () -> in(shorts).readChar());
        byte[] one = {(byte) 0x80};
        ckT("dis.readByte.signed", () -> in(one).readByte());
        ckT("dis.readUnsignedByte", () -> in(one).readUnsignedByte());
        ckT("dis.readBoolean.true", () -> in(new byte[] {1}).readBoolean());
        ckT("dis.readBoolean.zero", () -> in(new byte[] {0}).readBoolean());
        ckT("dis.readBoolean.two", () -> in(new byte[] {2}).readBoolean());
        ckT("dis.readLong", () -> in(new byte[] {
                (byte) 0x80, 0, 0, 0, 0, 0, 0, 0}).readLong());
        ckT("dis.readFloat", () -> in(new byte[] {0x3f, (byte) 0x80, 0, 0}).readFloat());
        ckT("dis.readDouble", () -> in(new byte[] {
                (byte) 0xc0, 4, 0, 0, 0, 0, 0, 0}).readDouble());

        // ---- the EOF contract, which is per-method -----------------------
        ckT("dis.readInt.eof", () -> in(new byte[] {1, 2}).readInt());
        ckT("dis.readByte.eof", () -> in(new byte[0]).readByte());
        ckT("dis.readUnsignedByte.eof", () -> in(new byte[0]).readUnsignedByte());
        ckT("dis.readBoolean.eof", () -> in(new byte[0]).readBoolean());
        ckT("dis.readLong.eof", () -> in(new byte[] {1}).readLong());
        // `read()` returns -1 at EOF; only the typed reads throw.
        ckT("dis.read.eof", () -> in(new byte[0]).read());
        ckT("dis.read.bytes.eof", () -> in(new byte[0]).read(new byte[4], 0, 4));
        ckT("dis.readFully.eof", () -> {
            in(new byte[] {1, 2}).readFully(new byte[4]);
            return "no-throw";
        });
        ckT("dis.readFully.exact", () -> {
            byte[] b = new byte[3];
            in(new byte[] {7, 8, 9}).readFully(b);
            return Arrays.toString(b);
        });
        ckT("dis.readFully.offset", () -> {
            byte[] b = new byte[5];
            in(new byte[] {7, 8}).readFully(b, 2, 2);
            return Arrays.toString(b);
        });

        // ---- readUTF round trips, including the ones that are not UTF-8 ----
        for (String sample : new String[] {"", "AB", "a\0b", "café", "日本",
                                           "😀", "mixed é 日 z"}) {
            byte[] enc = out("dos.writeUTF.rt[" + sample.length() + "]",
                    d -> d.writeUTF(sample));
            ckT("dis.readUTF.rt[" + sample.length() + "]", () -> in(enc).readUTF());
        }
        ckT("dis.readUTF.eof", () -> in(new byte[] {0, 5, 'a'}).readUTF());
        ckT("dis.readUTF.badContinuation", () -> in(new byte[] {0, 1, (byte) 0x80}).readUTF());

        // ---- skipBytes / available ----------------------------------------
        ckT("dis.skipBytes", () -> in(new byte[] {1, 2, 3, 4}).skipBytes(2));
        ckT("dis.skipBytes.past", () -> in(new byte[] {1, 2}).skipBytes(9));
        ckT("dis.skipBytes.negative", () -> in(new byte[] {1, 2}).skipBytes(-1));
        ckT("dis.available", () -> in(new byte[] {1, 2, 3}).available());
        ckT("dis.available.afterRead", () -> {
            DataInputStream d = in(new byte[] {1, 2, 3});
            d.readByte();
            return d.available();
        });

        // ---- a full round trip through a wrapped, non-BAIS stream ----------
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        try (DataOutputStream d = new DataOutputStream(new BufferedOutputStream(bo))) {
            d.writeInt(0x0a0b0c0d);
            d.writeUTF("round");
            d.writeDouble(3.5);
            d.writeBoolean(true);
        }
        ck("roundTrip.bytes", hex(bo.toByteArray()));
        try (DataInputStream d = new DataInputStream(
                new BufferedInputStream(new ByteArrayInputStream(bo.toByteArray())))) {
            ck("roundTrip.int", d.readInt());
            ck("roundTrip.utf", d.readUTF());
            ck("roundTrip.double", d.readDouble());
            ck("roundTrip.boolean", d.readBoolean());
            ck("roundTrip.eofNext", d.read());
        }

        System.out.println("PASS W4Data");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
