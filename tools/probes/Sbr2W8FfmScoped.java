import java.lang.foreign.*;
import java.nio.*;

/** SBR2 wave 8 lane FFM: the ScopedMemoryAccess get/put Byte/Short/Int/Long wrappers, reached through direct
 *  buffer views and through arena-backed ByteBuffers (the closed-arena ScopedAccessError path). One line per row. */
public class Sbr2W8FfmScoped {
    interface F { Object get() throws Throwable; }
    static void p(String tag, F f) {
        String v;
        try { v = String.valueOf(f.get()); }
        catch (Throwable e) { v = "THREW " + e.getClass().getName() + ": " + e.getMessage(); }
        System.out.println(tag + " |" + v + "|");
    }
    public static void main(String[] a) {
        for (ByteOrder bo : new ByteOrder[]{ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN}) {
            String o = bo.toString();
            p(o + ".dbb.byte", () -> { ByteBuffer b = ByteBuffer.allocateDirect(16).order(bo); b.put(3, (byte) -5); b.put((byte) 9); return b.get(3) + " " + b.get(0) + " " + b.position(); });
            p(o + ".dbb.short.view", () -> { ShortBuffer b = ByteBuffer.allocateDirect(16).order(bo).asShortBuffer(); b.put(1, (short) -2); b.put((short) 7); return b.get(1) + " " + b.get(0) + " " + b.remaining(); });
            p(o + ".dbb.int.view", () -> { IntBuffer b = ByteBuffer.allocateDirect(16).order(bo).asIntBuffer(); b.put(1, -2); b.put(7); return b.get(1) + " " + b.get(0) + " " + b.remaining(); });
            p(o + ".dbb.long.view", () -> { LongBuffer b = ByteBuffer.allocateDirect(32).order(bo).asLongBuffer(); b.put(1, -2L); b.put(1L << 40); return b.get(1) + " " + b.get(0) + " " + b.remaining(); });
            p(o + ".dbb.bulk.int", () -> { IntBuffer b = ByteBuffer.allocateDirect(64).order(bo).asIntBuffer(); b.put(new int[]{1, 2, 3, 4}); b.flip(); int[] d = new int[4]; b.get(d); return java.util.Arrays.toString(d); });
            p(o + ".dbb.bulk.long", () -> { LongBuffer b = ByteBuffer.allocateDirect(64).order(bo).asLongBuffer(); b.put(new long[]{1, 2, 3, 4}); b.flip(); long[] d = new long[4]; b.get(d); return java.util.Arrays.toString(d); });
            p(o + ".dbb.bulk.short", () -> { ShortBuffer b = ByteBuffer.allocateDirect(64).order(bo).asShortBuffer(); b.put(new short[]{1, 2, 3, 4}); b.flip(); short[] d = new short[4]; b.get(d); return java.util.Arrays.toString(d); });
            p(o + ".dbb.oob", () -> ByteBuffer.allocateDirect(4).order(bo).asIntBuffer().get(1));
            p(o + ".dbb.ro.put", () -> { ByteBuffer.allocateDirect(8).order(bo).asReadOnlyBuffer().asIntBuffer().put(0, 1); return "no"; });
            p(o + ".dbb.slice.long", () -> { ByteBuffer b = ByteBuffer.allocateDirect(32).order(bo); b.position(8); LongBuffer l = b.slice().order(bo).asLongBuffer(); l.put(0, 77L); return b.getLong(8) + " " + l.get(0); });
            p(o + ".dbb.dup.short", () -> { ByteBuffer b = ByteBuffer.allocateDirect(8).order(bo); ShortBuffer s = b.asShortBuffer(); s.put(0, (short) 513); return b.get(0) + " " + b.get(1); });
        }
        p("heap.view.int", () -> { IntBuffer b = ByteBuffer.allocate(16).asIntBuffer(); b.put(2, 9); return b.get(2); });
        p("arena.bb.byte", () -> { try (Arena x = Arena.ofConfined()) { ByteBuffer b = x.allocate(16).asByteBuffer(); b.put(2, (byte) 4); return b.get(2); } });
        p("arena.bb.int.view", () -> { try (Arena x = Arena.ofConfined()) { IntBuffer b = x.allocate(16).asByteBuffer().asIntBuffer(); b.put(1, 4); return b.get(1); } });
        p("arena.bb.long.view", () -> { try (Arena x = Arena.ofConfined()) { LongBuffer b = x.allocate(16).asByteBuffer().asLongBuffer(); b.put(1, 4L); return b.get(1); } });
        p("arena.bb.short.view", () -> { try (Arena x = Arena.ofConfined()) { ShortBuffer b = x.allocate(16).asByteBuffer().asShortBuffer(); b.put(1, (short) 4); return b.get(1); } });
        p("arena.closed.byte.get", () -> { Arena x = Arena.ofConfined(); ByteBuffer b = x.allocate(16).asByteBuffer(); x.close(); return b.get(0); });
        p("arena.closed.byte.put", () -> { Arena x = Arena.ofConfined(); ByteBuffer b = x.allocate(16).asByteBuffer(); x.close(); b.put(0, (byte) 1); return "no"; });
        p("arena.closed.int.get", () -> { Arena x = Arena.ofConfined(); IntBuffer b = x.allocate(16).asByteBuffer().asIntBuffer(); x.close(); return b.get(0); });
        p("arena.closed.int.put", () -> { Arena x = Arena.ofConfined(); IntBuffer b = x.allocate(16).asByteBuffer().asIntBuffer(); x.close(); b.put(0, 1); return "no"; });
        p("arena.closed.long.get", () -> { Arena x = Arena.ofConfined(); LongBuffer b = x.allocate(16).asByteBuffer().asLongBuffer(); x.close(); return b.get(0); });
        p("arena.closed.long.put", () -> { Arena x = Arena.ofConfined(); LongBuffer b = x.allocate(16).asByteBuffer().asLongBuffer(); x.close(); b.put(0, 1L); return "no"; });
        p("arena.closed.short.get", () -> { Arena x = Arena.ofConfined(); ShortBuffer b = x.allocate(16).asByteBuffer().asShortBuffer(); x.close(); return b.get(0); });
        p("arena.closed.short.put", () -> { Arena x = Arena.ofConfined(); ShortBuffer b = x.allocate(16).asByteBuffer().asShortBuffer(); x.close(); b.put(0, (short) 1); return "no"; });
        p("arena.thread.byte", () -> { try (Arena x = Arena.ofConfined()) { ByteBuffer b = x.allocate(16).asByteBuffer(); Object[] r = new Object[1]; Thread t = new Thread(() -> { try { r[0] = b.get(0); } catch (Throwable e) { r[0] = e.getClass().getName() + ": " + e.getMessage(); } }); t.start(); t.join(); return r[0]; } });
        p("arena.thread.long", () -> { try (Arena x = Arena.ofConfined()) { LongBuffer b = x.allocate(16).asByteBuffer().asLongBuffer(); Object[] r = new Object[1]; Thread t = new Thread(() -> { try { r[0] = b.get(0); } catch (Throwable e) { r[0] = e.getClass().getName() + ": " + e.getMessage(); } }); t.start(); t.join(); return r[0]; } });
        p("arena.shared.thread.int", () -> { try (Arena x = Arena.ofShared()) { IntBuffer b = x.allocate(16).asByteBuffer().asIntBuffer(); b.put(0, 5); Object[] r = new Object[1]; Thread t = new Thread(() -> { r[0] = b.get(0); }); t.start(); t.join(); return r[0]; } });
    }
}
