import java.lang.foreign.*;
import java.lang.invoke.*;
import java.nio.*;

public class Sbr2W9Ffm2OfBuffer {
    static void row(String n, Object v) { System.out.println(n + " |" + String.valueOf(v).replaceAll("0x[0-9a-fA-F]+", "0xA").replaceAll("@[0-9a-f]+", "@X") + "|"); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) {
        String only = System.getenv("FFM2_ONLY");
        if (only != null && !n.startsWith(only)) return;
        try { row(n, r.run()); } catch (Throwable e) { row(n, "THREW " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    public static void main(String[] a) throws Throwable {
        t("ofBuffer.size", () -> MemorySegment.ofBuffer(ByteBuffer.allocateDirect(32)).byteSize());
        t("ofBuffer.isNative", () -> MemorySegment.ofBuffer(ByteBuffer.allocateDirect(32)).isNative());
        t("ofBuffer.get.byte", () -> { ByteBuffer b = ByteBuffer.allocateDirect(16); b.put(3, (byte) 7); return MemorySegment.ofBuffer(b).get(ValueLayout.JAVA_BYTE, 3); });
        t("ofBuffer.set.get.int", () -> { ByteBuffer b = ByteBuffer.allocateDirect(16).order(ByteOrder.nativeOrder()); MemorySegment s = MemorySegment.ofBuffer(b); s.set(ValueLayout.JAVA_INT, 4, 0x01020304); return b.getInt(4) + " " + s.get(ValueLayout.JAVA_INT, 4); });
        t("ofBuffer.set.get.long", () -> { ByteBuffer b = ByteBuffer.allocateDirect(16).order(ByteOrder.nativeOrder()); MemorySegment s = MemorySegment.ofBuffer(b); s.set(ValueLayout.JAVA_LONG, 8, 0x0102030405060708L); return b.getLong(8) + " " + s.get(ValueLayout.JAVA_LONG, 8); });
        t("ofBuffer.slice.pos", () -> { ByteBuffer b = ByteBuffer.allocateDirect(16); b.put(5, (byte) 9); b.position(4); MemorySegment s = MemorySegment.ofBuffer(b); return s.byteSize() + " " + s.get(ValueLayout.JAVA_BYTE, 1); });
        t("ofBuffer.fill", () -> { ByteBuffer b = ByteBuffer.allocateDirect(8); MemorySegment s = MemorySegment.ofBuffer(b); s.fill((byte) 5); return b.get(0) + " " + b.get(7); });
        t("ofBuffer.copy.out", () -> { ByteBuffer b = ByteBuffer.allocateDirect(8); for (int i = 0; i < 8; i++) b.put(i, (byte) i); byte[] o = new byte[8]; MemorySegment.copy(MemorySegment.ofBuffer(b), ValueLayout.JAVA_BYTE, 0, o, 0, 8); return java.util.Arrays.toString(o); });
        t("ofBuffer.copy.in", () -> { ByteBuffer b = ByteBuffer.allocateDirect(8); MemorySegment.copy(new byte[]{1,2,3,4,5,6,7,8}, 0, MemorySegment.ofBuffer(b), ValueLayout.JAVA_BYTE, 0, 8); return b.get(0) + " " + b.get(7); });
        t("ofBuffer.mismatch", () -> { ByteBuffer b = ByteBuffer.allocateDirect(8); b.put(3, (byte) 1); return MemorySegment.ofBuffer(b).mismatch(MemorySegment.ofArray(new byte[8])); });
        t("ofBuffer.heap", () -> { ByteBuffer b = ByteBuffer.allocate(8); b.put(2, (byte) 4); MemorySegment s = MemorySegment.ofBuffer(b); return s.isNative() + " " + s.get(ValueLayout.JAVA_BYTE, 2); });
        t("ofBuffer.ro.set", () -> { ByteBuffer b = ByteBuffer.allocateDirect(8).asReadOnlyBuffer(); MemorySegment.ofBuffer(b).set(ValueLayout.JAVA_BYTE, 0, (byte) 1); return "no"; });
        t("ofBuffer.oob", () -> MemorySegment.ofBuffer(ByteBuffer.allocateDirect(8)).get(ValueLayout.JAVA_LONG, 4));
        t("ofBuffer.vh", () -> { ByteBuffer b = ByteBuffer.allocateDirect(16).order(ByteOrder.nativeOrder()); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); MemorySegment s = MemorySegment.ofBuffer(b); vh.set(s, 0L, 77); return b.getInt(0) + " " + (int) vh.get(s, 0L); });
        t("ofBuffer.asByteBuffer", () -> { ByteBuffer b = ByteBuffer.allocateDirect(8); b.put(1, (byte) 6); ByteBuffer c = MemorySegment.ofBuffer(b).asByteBuffer(); return c.isDirect() + " " + c.get(1); });
        t("ofBuffer.toArray", () -> { ByteBuffer b = ByteBuffer.allocateDirect(4); b.put(0, (byte) 1); b.put(3, (byte) 4); return java.util.Arrays.toString(MemorySegment.ofBuffer(b).toArray(ValueLayout.JAVA_BYTE)); });
        t("ofBuffer.getString", () -> { ByteBuffer b = ByteBuffer.allocateDirect(8); b.put(0, (byte) 'h'); b.put(1, (byte) 'i'); return MemorySegment.ofBuffer(b).getString(0); });
        t("ofBuffer.arenaBuf", () -> { try (Arena ar = Arena.ofConfined()) { MemorySegment s = ar.allocate(8); s.set(ValueLayout.JAVA_BYTE, 0, (byte) 3); ByteBuffer b = s.asByteBuffer(); MemorySegment s2 = MemorySegment.ofBuffer(b); return s2.get(ValueLayout.JAVA_BYTE, 0) + " " + s2.byteSize(); } });
    }
}
