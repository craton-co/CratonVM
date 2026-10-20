// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// SBR wave 1, lane `bytes`: the family probe for RETIRED_SHADOW_SBR_BYTES_TRIPLES.
//
// Exercises every retired row's surface -- jdk.internal.util.ByteArray (directly
// and through DataInput/DataOutput, ObjectStreams, RandomAccessFile, UUID),
// AbstractMap.SimpleEntry, the StringBuilder-keyed ASB helpers, Bits$1.getName
// and the four JavaNioAccess (Buffer$2) rows -- prints the FIRST-run value of
// every line, then re-runs each section inside a hot loop and counts lines that
// differ from the first run (`drift`), so cold == warm is asserted in the same
// process. Diff the output against HotSpot 25.
//
//   X="--add-exports java.base/jdk.internal.util=ALL-UNNAMED
//      --add-exports java.base/jdk.internal.access=ALL-UNNAMED
//      --add-exports java.base/jdk.internal.misc=ALL-UNNAMED
//      --add-exports java.base/sun.nio.ch=ALL-UNNAMED"
//   javac $X -d out SbrW1Bytes.java && java $X -cp out SbrW1Bytes [scale]
import java.io.*;
import java.lang.management.BufferPoolMXBean;
import java.lang.management.ManagementFactory;
import java.nio.*;
import java.util.*;
import java.util.function.Supplier;
import jdk.internal.access.JavaNioAccess;
import jdk.internal.access.SharedSecrets;
import jdk.internal.util.ByteArray;

public class SbrW1Bytes {
    static final long ODD_NAN_D = 0x7ff0000000000001L;
    static final int ODD_NAN_F = 0x7f800001;

    static String hex(byte[] b, int off, int len) {
        StringBuilder sb = new StringBuilder();
        for (int i = off; i < off + len; i++) sb.append(String.format("%02x", b[i] & 0xff));
        return sb.toString();
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + "(" + t.getMessage() + ")";
    }

    interface Body { Object run() throws Throwable; }

    static String cap(Body b) {
        try {
            Object o = b.run();
            return String.valueOf(o);
        } catch (Throwable t) {
            return "THROW " + ex(t);
        }
    }

    // ---- 1. ByteArray, called directly -------------------------------------
    static List<String> byteArray() {
        List<String> out = new ArrayList<>();
        byte[] a = new byte[16];
        for (int i = 0; i < a.length; i++) a[i] = (byte) (0x81 + i * 17);
        out.add("getBoolean " + ByteArray.getBoolean(a, 3) + " " + ByteArray.getBoolean(new byte[1], 0));
        out.add("getChar " + (int) ByteArray.getChar(a, 1));
        out.add("getShort " + ByteArray.getShort(a, 2));
        out.add("getUnsignedShort " + ByteArray.getUnsignedShort(a, 2));
        out.add("getInt " + ByteArray.getInt(a, 5));
        out.add("getFloat " + Float.floatToRawIntBits(ByteArray.getFloat(a, 4)));
        out.add("getFloatRaw " + Float.floatToRawIntBits(ByteArray.getFloatRaw(a, 4)));
        out.add("getLong " + ByteArray.getLong(a, 7));
        out.add("getDouble " + Double.doubleToRawLongBits(ByteArray.getDouble(a, 8)));
        out.add("getDoubleRaw " + Double.doubleToRawLongBits(ByteArray.getDoubleRaw(a, 8)));
        byte[] w = new byte[16];
        ByteArray.setBoolean(w, 0, true);
        ByteArray.setChar(w, 1, '뻯');
        ByteArray.setShort(w, 3, (short) -2);
        ByteArray.setUnsignedShort(w, 5, 0xFFFE);
        ByteArray.setInt(w, 7, 0xCAFEBABE);
        ByteArray.setBoolean(w, 11, false);
        out.add("set.mixed " + hex(w, 0, 16));
        ByteArray.setLong(w, 0, 0x0123456789ABCDEFL);
        ByteArray.setDouble(w, 8, Math.PI);
        out.add("set.long+double " + hex(w, 0, 16));
        ByteArray.setFloat(w, 0, 1.5f);
        ByteArray.setFloatRaw(w, 4, -0.0f);
        out.add("set.float " + hex(w, 0, 8));
        // The canonicalising pair: setDouble/setFloat go through doubleToLongBits /
        // floatToIntBits and collapse a non-canonical NaN; the *Raw pair keeps it.
        ByteArray.setDouble(w, 0, Double.longBitsToDouble(ODD_NAN_D));
        ByteArray.setDoubleRaw(w, 8, Double.longBitsToDouble(ODD_NAN_D));
        out.add("set.nan.double " + hex(w, 0, 16));
        ByteArray.setFloat(w, 0, Float.intBitsToFloat(ODD_NAN_F));
        ByteArray.setFloatRaw(w, 4, Float.intBitsToFloat(ODD_NAN_F));
        out.add("set.nan.float " + hex(w, 0, 8));
        // Bounds: the JDK's VarHandle view throws from Preconditions.
        byte[] s = new byte[6];
        out.add("oob.getInt.end " + cap(() -> ByteArray.getInt(s, 3)));
        out.add("oob.getInt.neg " + cap(() -> ByteArray.getInt(s, -1)));
        out.add("oob.getLong " + cap(() -> ByteArray.getLong(s, 0)));
        out.add("oob.getBoolean " + cap(() -> ByteArray.getBoolean(s, 6)));
        out.add("oob.setShort " + cap(() -> { ByteArray.setShort(s, 5, (short) 1); return hex(s, 0, 6); }));
        out.add("oob.setLong " + cap(() -> { ByteArray.setLong(s, -2, 1L); return hex(s, 0, 6); }));
        out.add("oob.after " + hex(s, 0, 6));
        out.add("npe.get " + cap(() -> ByteArray.getInt(null, 0)));
        out.add("npe.set " + cap(() -> { ByteArray.setInt(null, 0, 1); return "no"; }));
        // Every width, both ends, both directions; the boolean pair is a plain
        // array access in the JDK, the rest a VarHandle view.
        byte[] t = new byte[6];
        out.add("oob.w " + cap(() -> ByteArray.getBoolean(t, -1)) + " | " + cap(() -> ByteArray.getChar(t, 5))
                + " | " + cap(() -> ByteArray.getShort(t, -1)) + " | " + cap(() -> ByteArray.getUnsignedShort(t, 5))
                + " | " + cap(() -> ByteArray.getFloat(t, 3)) + " | " + cap(() -> ByteArray.getFloatRaw(t, -3))
                + " | " + cap(() -> ByteArray.getDouble(t, 0)) + " | " + cap(() -> ByteArray.getDoubleRaw(new byte[8], 1))
                + " | " + cap(() -> ByteArray.getLong(new byte[3], -9)) + " | " + cap(() -> ByteArray.getInt(new byte[0], 0)));
        out.add("oob.s " + cap(() -> { ByteArray.setBoolean(t, 6, true); return "no"; })
                + " | " + cap(() -> { ByteArray.setChar(t, 5, 'x'); return "no"; })
                + " | " + cap(() -> { ByteArray.setUnsignedShort(t, -1, 1); return "no"; })
                + " | " + cap(() -> { ByteArray.setInt(t, 3, 1); return "no"; })
                + " | " + cap(() -> { ByteArray.setFloat(t, 3, 1f); return "no"; })
                + " | " + cap(() -> { ByteArray.setFloatRaw(t, Integer.MIN_VALUE, 1f); return "no"; })
                + " | " + cap(() -> { ByteArray.setDouble(t, 0, 1d); return "no"; })
                + " | " + cap(() -> { ByteArray.setDoubleRaw(t, 0, 1d); return "no"; })
                + " | " + hex(t, 0, 6));
        out.add("npe.w " + cap(() -> ByteArray.getBoolean(null, 0)) + " | " + cap(() -> { ByteArray.setBoolean(null, 0, true); return "no"; })
                + " | " + cap(() -> ByteArray.getChar(null, 0)) + " | " + cap(() -> ByteArray.getDouble(null, 0))
                + " | " + cap(() -> { ByteArray.setFloat(null, 0, 1f); return "no"; })
                + " | " + cap(() -> { ByteArray.setLong(null, -5, 1L); return "no"; }));
        // Edge values in range: last valid offset of every width.
        byte[] e = new byte[9];
        for (int i = 0; i < e.length; i++) e[i] = (byte) (0xF0 | i);
        out.add("edge " + ByteArray.getBoolean(e, 8) + " " + (int) ByteArray.getChar(e, 7) + " " + ByteArray.getShort(e, 7)
                + " " + ByteArray.getUnsignedShort(e, 7) + " " + ByteArray.getInt(e, 5) + " " + ByteArray.getLong(e, 1)
                + " " + Float.floatToRawIntBits(ByteArray.getFloat(e, 5)) + " " + Double.doubleToRawLongBits(ByteArray.getDoubleRaw(e, 1)));
        ByteArray.setShort(e, 7, (short) 0x1234);
        ByteArray.setUnsignedShort(e, 0, 0x12345);
        ByteArray.setChar(e, 2, (char) 0xABCD);
        out.add("edge.set " + hex(e, 0, 9));
        return out;
    }

    // ---- 2. The JDK's own callers of ByteArray -------------------------------
    static List<String> dataStreams() throws IOException {
        List<String> out = new ArrayList<>();
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        DataOutputStream d = new DataOutputStream(bo);
        d.writeBoolean(true);
        d.writeShort(-12345);
        d.writeChar('€');
        d.writeInt(0x89ABCDEF);
        d.writeLong(-0x0123456789ABCDEFL);
        d.writeFloat(Float.intBitsToFloat(ODD_NAN_F));
        d.writeDouble(Double.longBitsToDouble(ODD_NAN_D));
        d.writeFloat(3.25f);
        d.writeDouble(-1e300);
        d.writeUTF("héllo");
        d.flush();
        byte[] b = bo.toByteArray();
        out.add("dos " + b.length + " " + hex(b, 0, b.length));
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(b));
        out.add("dis " + in.readBoolean() + " " + in.readShort() + " " + (int) in.readChar() + " "
                + in.readInt() + " " + in.readLong() + " "
                + Integer.toHexString(Float.floatToRawIntBits(in.readFloat())) + " "
                + Long.toHexString(Double.doubleToRawLongBits(in.readDouble())) + " "
                + in.readFloat() + " " + in.readDouble() + " " + in.readUTF());
        DataInputStream in2 = new DataInputStream(new ByteArrayInputStream(new byte[] {1, 2, 3}));
        out.add("dis.eof " + cap(in2::readInt));
        DataInputStream in3 = new DataInputStream(new ByteArrayInputStream(new byte[] {(byte) 0xff, (byte) 0xfe}));
        out.add("dis.ushort " + in3.readUnsignedShort());
        return out;
    }

    static class Prims implements Serializable {
        private static final long serialVersionUID = 42L;
        boolean z = true; byte b = -3; char c = 'é'; short s = -300;
        int i = 0x7fedcba9; long j = Long.MIN_VALUE + 7; float f = -2.5f;
        double dd = Double.longBitsToDouble(ODD_NAN_D); float fn = Float.intBitsToFloat(ODD_NAN_F);
        double d2 = 6.02e23;
        String render() {
            return z + "," + b + "," + (int) c + "," + s + "," + i + "," + j + "," + f + ","
                    + Long.toHexString(Double.doubleToRawLongBits(dd)) + ","
                    + Integer.toHexString(Float.floatToRawIntBits(fn)) + "," + d2;
        }
    }

    static List<String> serial() throws Exception {
        List<String> out = new ArrayList<>();
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        try (ObjectOutputStream oo = new ObjectOutputStream(bo)) {
            oo.writeObject(new Prims());
            oo.writeObject(UUID.fromString("f81d4fae-7dec-11d0-a765-00a0c91e6bf6"));
            oo.writeInt(77);
            oo.writeDouble(Double.longBitsToDouble(ODD_NAN_D));
        }
        byte[] b = bo.toByteArray();
        CRC32Like crc = new CRC32Like();
        out.add("oos.len " + b.length + " crc " + crc.of(b));
        try (ObjectInputStream oi = new ObjectInputStream(new ByteArrayInputStream(b))) {
            Prims p = (Prims) oi.readObject();
            UUID u = (UUID) oi.readObject();
            out.add("ois " + p.render() + " " + u + " " + oi.readInt() + " "
                    + Long.toHexString(Double.doubleToRawLongBits(oi.readDouble())));
        }
        return out;
    }

    static final class CRC32Like {
        String of(byte[] b) {
            java.util.zip.CRC32 c = new java.util.zip.CRC32();
            c.update(b);
            return Long.toHexString(c.getValue());
        }
    }

    static List<String> raf() throws IOException {
        List<String> out = new ArrayList<>();
        File f = File.createTempFile("sbrw1bytes", ".bin");
        f.deleteOnExit();
        try (RandomAccessFile r = new RandomAccessFile(f, "rw")) {
            r.writeInt(-7);
            r.writeLong(0x1122334455667788L);
            r.writeShort(0xBEEF);
            r.writeDouble(Double.longBitsToDouble(ODD_NAN_D));
            r.writeFloat(Float.intBitsToFloat(ODD_NAN_F));
            r.seek(0);
            out.add("raf " + r.readInt() + " " + Long.toHexString(r.readLong()) + " " + r.readShort()
                    + " " + Long.toHexString(Double.doubleToRawLongBits(r.readDouble()))
                    + " " + Integer.toHexString(Float.floatToRawIntBits(r.readFloat())) + " len " + r.length());
        }
        f.delete();
        return out;
    }

    static List<String> uuid() {
        List<String> out = new ArrayList<>();
        UUID u = new UUID(0x0123456789ABCDEFL, 0xFEDCBA9876543210L);
        out.add("uuid " + u + " " + UUID.fromString(u.toString()).equals(u) + " " + u.hashCode());
        out.add("uuid.name " + UUID.nameUUIDFromBytes("craton".getBytes()));
        return out;
    }

    // ---- 3. AbstractMap.SimpleEntry -------------------------------------------
    static List<String> simpleEntry() {
        List<String> out = new ArrayList<>();
        AbstractMap.SimpleEntry<String, Integer> e = new AbstractMap.SimpleEntry<>("k", 7);
        out.add("se " + e.getKey() + " " + e.getValue() + " " + e.hashCode() + " " + e);
        out.add("se.set " + e.setValue(8) + " -> " + e.getValue() + " " + e.hashCode());
        AbstractMap.SimpleEntry<String, Integer> n = new AbstractMap.SimpleEntry<>(null, null);
        out.add("se.null " + n.getKey() + " " + n.getValue() + " " + n.hashCode() + " " + n);
        out.add("se.eq " + e.equals(Map.entry("k", 8)) + " " + e.equals(new AbstractMap.SimpleEntry<>("k", 8))
                + " " + e.equals(Map.entry("k", 9)) + " " + e.equals("k=8") + " " + e.equals(null) + " " + n.equals(n));
        AbstractMap.SimpleEntry<String, Integer> c = new AbstractMap.SimpleEntry<>(Map.entry("x", 1));
        out.add("se.copy " + c + " " + c.hashCode());
        // Entries a map hands out: whatever class they are, their contract holds.
        TreeMap<String, Integer> tm = new TreeMap<>(Map.of("a", 1, "b", 2));
        StringBuilder sb = new StringBuilder();
        for (Map.Entry<String, Integer> me : new AbstractMap<String, Integer>() {
                public Set<Map.Entry<String, Integer>> entrySet() {
                    Set<Map.Entry<String, Integer>> s = new LinkedHashSet<>();
                    for (Map.Entry<String, Integer> x : tm.entrySet()) s.add(new AbstractMap.SimpleEntry<>(x));
                    return s;
                }
            }.entrySet()) {
            sb.append(me.getKey()).append(me.getValue()).append(me.hashCode()).append(';');
        }
        out.add("se.view " + sb);
        LinkedHashMap<String, Integer> lhm = new LinkedHashMap<>();
        lhm.put("one", 1); lhm.put("two", 2);
        for (Map.Entry<String, Integer> me : lhm.entrySet()) me.setValue(me.getValue() * 10);
        out.add("lhm.writethrough " + lhm + " " + lhm.entrySet().iterator().next().hashCode());
        HashMap<String, Integer> hm = new HashMap<>(Map.of("p", 3));
        for (Map.Entry<String, Integer> me : hm.entrySet()) me.setValue(33);
        out.add("hm.writethrough " + hm);
        return out;
    }

    // ---- 4. StringBuilder rows inherited from AbstractStringBuilder ------------
    static List<String> stringBuilder() {
        List<String> out = new ArrayList<>();
        StringBuilder a = new StringBuilder("abc");
        StringBuilder b = new StringBuilder("déf");
        a.append(b);                       // append(AbstractStringBuilder)
        a.append((CharSequence) null);     // appendNull
        a.append((String) null);
        a.append((StringBuffer) null);
        a.append(new StringBuffer("sb"));
        out.add("sb.append " + a + " " + a.length());
        StringBuilder u = new StringBuilder("中");
        u.append(new StringBuilder("x"));   // UTF16 + LATIN1
        StringBuilder l = new StringBuilder("x");
        l.append(new StringBuilder("中")); // LATIN1 inflated by UTF16 arg
        out.add("sb.coder " + u + " " + l + " " + u.length() + " " + l.length());
        StringBuilder r = new StringBuilder();
        r.repeat('z', 3).repeat(0x1F600, 2).repeat("ab", 2).repeat('é', 1);   // repeat(CI) via repeat(II)
        out.add("sb.repeat " + r + " " + r.length());
        // getValue/getCoder consumers.
        out.add("sb.string " + new String(a) + " " + String.valueOf(u) + " " + "abcdéfnullnullnullsb".contentEquals(a)
                + " " + a.compareTo(new StringBuilder("abcd")) + " " + u.compareTo(l)
                + " " + String.join("|", a, u, l) + " " + a.indexOf("null") + " " + u.lastIndexOf("x"));
        StringBuilder big = new StringBuilder();
        for (int i = 0; i < 40; i++) big.append((char) ('a' + i % 26));
        StringBuilder host = new StringBuilder("<");
        host.append(big).append(">").insert(1, big.subSequence(0, 3));
        out.add("sb.big " + host + " " + host.toString().hashCode());
        return out;
    }

    // ---- 5. JavaNioAccess (Buffer$2) and Bits$1 --------------------------------
    static List<String> nioAccess() {
        List<String> out = new ArrayList<>();
        JavaNioAccess nio = SharedSecrets.getJavaNioAccess();
        byte[] arr = new byte[8];
        ByteBuffer heap = ByteBuffer.wrap(arr);
        out.add("base.heap " + (nio.getBufferBase(heap) == arr));
        out.add("base.slice " + (nio.getBufferBase(heap.slice(2, 3)) == arr));
        int[] ia = new int[4];
        out.add("base.int " + (nio.getBufferBase(IntBuffer.wrap(ia)) == ia));
        ByteBuffer direct = ByteBuffer.allocateDirect(32);
        out.add("base.direct " + nio.getBufferBase(direct));
        out.add("base.ro " + (nio.getBufferBase(heap.asReadOnlyBuffer()) == arr));
        StringBuilder sh = new StringBuilder();
        for (Buffer bf : new Buffer[] {
                heap, direct, CharBuffer.allocate(2), ShortBuffer.allocate(2), IntBuffer.allocate(2),
                FloatBuffer.allocate(2), LongBuffer.allocate(2), DoubleBuffer.allocate(2),
                heap.asIntBuffer(), direct.asLongBuffer(), direct.order(ByteOrder.LITTLE_ENDIAN).asCharBuffer(),
                heap.asDoubleBuffer(), CharBuffer.wrap("abc"), direct.asReadOnlyBuffer().asFloatBuffer() }) {
            sh.append(nio.scaleShifts(bf));
        }
        out.add("scaleShifts " + sh);
        out.add("scaleShifts.null " + cap(() -> nio.scaleShifts(null)));
        direct.putLong(0, 0x0102030405060708L).putInt(8, 0x0A0B0C0D);
        long addr = nio.getBufferAddress(direct);
        ByteBuffer v2 = nio.newDirectByteBuffer(addr, 12);
        out.add("ndbb2 " + v2.getClass().getName() + " cap " + v2.capacity() + " lim " + v2.limit() + " pos "
                + v2.position() + " " + Long.toHexString(v2.getLong(0)) + " " + Integer.toHexString(v2.getInt(8))
                + " direct " + v2.isDirect() + " ro " + v2.isReadOnly() + " order " + v2.order()
                + " sameAddr " + (nio.getBufferAddress(v2) == addr));
        ByteBuffer v4 = nio.newDirectByteBuffer(addr + 8, 4, direct, null);
        out.add("ndbb4 " + v4.getClass().getName() + " cap " + v4.capacity() + " "
                + Integer.toHexString(v4.getInt(0)) + " att " + (((sun.nio.ch.DirectBuffer) v4).attachment() == direct)
                + " sameAddr " + (nio.getBufferAddress(v4) == addr + 8));
        v4.putInt(0, 0x11223344);
        out.add("ndbb4.write " + Integer.toHexString(direct.getInt(8)));
        // Bits$1 -- the JDK's own direct pool object.
        out.add("pool.name " + jdk.internal.misc.VM.getBufferPools().stream()
                .map(p -> p.getName()).sorted().toList());
        return out;
    }

    static List<String> mxbean() {
        List<String> out = new ArrayList<>();
        ByteBuffer keep = ByteBuffer.allocateDirect(1 << 16);
        List<String> names = new ArrayList<>();
        long directCount = -1;
        for (BufferPoolMXBean b : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) {
            names.add(b.getName());
            if (b.getName().equals("direct")) directCount = b.getCount();
        }
        Collections.sort(names);
        out.add("mx.names " + names + " directCount>0 " + (directCount > 0) + " " + keep.capacity());
        return out;
    }

    // ---- driver ----------------------------------------------------------------
    interface Section { List<String> run() throws Exception; }

    static int drift = 0;

    // The hot loop compares exception CLASSES, not messages: HotSpot's C2 drops
    // the message of a hot implicit exception (OmitStackTraceInFastThrow), so a
    // message is a first-run fact only. The first run prints it in full.
    static List<String> norm(List<String> l) {
        List<String> o = new ArrayList<>(l.size());
        for (String s : l) o.add(s.replaceAll("(THROW [\\w.$]+)\\([^|]*\\)", "$1"));
        return o;
    }

    static void section(String name, Section s, int hot) {
        List<String> first;
        try {
            first = s.run();
        } catch (Throwable t) {
            first = List.of("SECTION THROW " + ex(t));
        }
        for (String line : first) System.out.println(name + ": " + line);
        int d = 0;
        String firstDiff = null;
        List<String> firstN = norm(first);
        for (int i = 0; i < hot; i++) {
            List<String> again;
            try {
                again = s.run();
            } catch (Throwable t) {
                again = List.of("SECTION THROW " + ex(t));
            }
            if (!norm(again).equals(firstN)) {
                d++;
                if (firstDiff == null) firstDiff = "iter " + i + ": " + again;
            }
        }
        drift += d;
        System.out.println(name + ": drift " + d + "/" + hot + (firstDiff == null ? "" : " FIRST " + firstDiff));
    }

    public static void main(String[] args) throws Exception {
        int scale = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        section("ByteArray", SbrW1Bytes::byteArray, 20000 * scale);
        section("DataStreams", SbrW1Bytes::dataStreams, 5000 * scale);
        section("Serial", SbrW1Bytes::serial, 300 * scale);
        section("RAF", SbrW1Bytes::raf, 50 * scale);
        section("UUID", SbrW1Bytes::uuid, 5000 * scale);
        section("SimpleEntry", SbrW1Bytes::simpleEntry, 5000 * scale);
        section("StringBuilder", SbrW1Bytes::stringBuilder, 5000 * scale);
        section("NioAccess", SbrW1Bytes::nioAccess, 3000 * scale);
        section("MXBean", SbrW1Bytes::mxbean, 20 * scale);
        System.out.println("TOTAL drift " + drift);
    }
}
