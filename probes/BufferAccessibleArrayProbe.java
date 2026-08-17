import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.IntBuffer;

/**
 * The three-way `hasArray` / `array` / `arrayOffset` split, and the
 * read-only / bounds exception classes around it.
 *
 * `java.nio.CharBuffer.array()` (JDK 25 source, java.base/java/nio/CharBuffer.java
 * L1513) is
 *
 *   if (hb == null)     throw new UnsupportedOperationException();
 *   if (isReadOnly)     throw new ReadOnlyBufferException();
 *   return hb;
 *
 * — so `hasArray() == false` covers TWO states that throw DIFFERENT classes,
 * and `arrayOffset()` repeats the identical split. This probe records the
 * class HotSpot raises for every cell of that table, plus the read-only and
 * bounds families around it, so a single "not accessible" branch cannot be
 * mistaken for the contract.
 */
public class BufferAccessibleArrayProbe {

    static String cls(Throwable t) {
        return t == null ? "none" : t.getClass().getName();
    }

    interface Op { Object run() throws Throwable; }

    static void row(String what, Op op) {
        Throwable t = null;
        Object v = null;
        try {
            v = op.run();
        } catch (Throwable x) {
            t = x;
        }
        System.out.println(what + "\t" + (t == null ? "OK " + v : cls(t))
                + (t != null && t.getMessage() != null ? "\tmsg=" + t.getMessage() : ""));
    }

    public static void main(String[] a) {
        // ---- the three CharBuffer states -----------------------------------
        CharBuffer scb = CharBuffer.wrap("abcd");              // StringCharBuffer: hb == null, read-only
        CharBuffer hcb = CharBuffer.allocate(4);               // HeapCharBuffer:   hb != null, writable
        CharBuffer rcb = hcb.asReadOnlyBuffer();               // HeapCharBufferR:  hb != null, read-only

        System.out.println("== CharBuffer states ==");
        row("scb.getClass", () -> scb.getClass().getName());
        row("hcb.getClass", () -> hcb.getClass().getName());
        row("rcb.getClass", () -> rcb.getClass().getName());
        row("scb.isReadOnly", () -> scb.isReadOnly());
        row("hcb.isReadOnly", () -> hcb.isReadOnly());
        row("rcb.isReadOnly", () -> rcb.isReadOnly());
        row("scb.isDirect", () -> scb.isDirect());
        row("rcb.isDirect", () -> rcb.isDirect());
        row("scb.hasArray", () -> scb.hasArray());
        row("hcb.hasArray", () -> hcb.hasArray());
        row("rcb.hasArray", () -> rcb.hasArray());
        row("scb.array", () -> scb.array().length);
        row("hcb.array", () -> hcb.array().length);
        row("rcb.array", () -> rcb.array().length);
        row("scb.arrayOffset", () -> scb.arrayOffset());
        row("hcb.arrayOffset", () -> hcb.arrayOffset());
        row("rcb.arrayOffset", () -> rcb.arrayOffset());

        // ---- read-only mutators --------------------------------------------
        System.out.println("== CharBuffer read-only mutators ==");
        row("scb.put(0,'x')", () -> scb.put(0, 'x'));
        row("scb.put('x')", () -> scb.put('x'));
        row("scb.put(char[])", () -> scb.put(new char[] { 'x' }));
        row("scb.put(String)", () -> scb.put("x"));
        row("scb.compact", () -> scb.compact());
        row("rcb.put(0,'x')", () -> rcb.put(0, 'x'));
        row("rcb.put('x')", () -> rcb.put('x'));
        row("rcb.compact", () -> rcb.compact());

        // ---- read-only VIEWS are still legal -------------------------------
        System.out.println("== CharBuffer read-only views ==");
        row("scb.duplicate.isReadOnly", () -> scb.duplicate().isReadOnly());
        row("scb.slice.isReadOnly", () -> scb.slice().isReadOnly());
        row("scb.asReadOnlyBuffer.isReadOnly", () -> scb.asReadOnlyBuffer().isReadOnly());
        row("rcb.duplicate.getClass", () -> rcb.duplicate().getClass().getName());
        row("rcb.duplicate.hasArray", () -> rcb.duplicate().hasArray());
        row("hcb.asReadOnlyBuffer.getClass", () -> hcb.asReadOnlyBuffer().getClass().getName());
        row("hcb.duplicate.isReadOnly", () -> hcb.duplicate().isReadOnly());

        // ---- ByteBuffer: the same table, plus the DIRECT arm ----------------
        ByteBuffer hbb = ByteBuffer.allocate(8);
        ByteBuffer dbb = ByteBuffer.allocateDirect(8);
        ByteBuffer rbb = hbb.asReadOnlyBuffer();
        ByteBuffer rdb = dbb.asReadOnlyBuffer();
        System.out.println("== ByteBuffer states ==");
        row("hbb.getClass", () -> hbb.getClass().getName());
        row("dbb.getClass", () -> dbb.getClass().getName());
        row("rbb.getClass", () -> rbb.getClass().getName());
        row("rdb.getClass", () -> rdb.getClass().getName());
        row("hbb.hasArray", () -> hbb.hasArray());
        row("dbb.hasArray", () -> dbb.hasArray());
        row("rbb.hasArray", () -> rbb.hasArray());
        row("rdb.hasArray", () -> rdb.hasArray());
        row("hbb.array", () -> hbb.array().length);
        row("dbb.array", () -> dbb.array().length);
        row("rbb.array", () -> rbb.array().length);
        row("rdb.array", () -> rdb.array().length);
        row("hbb.arrayOffset", () -> hbb.arrayOffset());
        row("dbb.arrayOffset", () -> dbb.arrayOffset());
        row("rbb.arrayOffset", () -> rbb.arrayOffset());
        row("rdb.arrayOffset", () -> rdb.arrayOffset());
        row("rbb.put(0,(byte)1)", () -> rbb.put(0, (byte) 1));
        row("rbb.put((byte)1)", () -> rbb.put((byte) 1));
        row("rbb.put(byte[])", () -> rbb.put(new byte[] { 1 }));
        row("rbb.compact", () -> rbb.compact());
        row("rbb.putInt(0,1)", () -> rbb.putInt(0, 1));
        row("dbb.array", () -> dbb.array().length);
        row("hbb.slice.arrayOffset", () -> hbb.slice().arrayOffset());
        row("hbb.position(2).slice.arrayOffset", () -> hbb.position(2).slice().arrayOffset());

        // ---- typed VIEW buffers: hb == null, so UOE not ROBE ----------------
        System.out.println("== typed views ==");
        ByteBuffer fresh = ByteBuffer.allocate(8);
        IntBuffer vib = fresh.asIntBuffer();
        IntBuffer rvib = fresh.asReadOnlyBuffer().asIntBuffer();
        row("vib.getClass", () -> vib.getClass().getName());
        row("vib.hasArray", () -> vib.hasArray());
        row("vib.array", () -> vib.array().length);
        row("vib.arrayOffset", () -> vib.arrayOffset());
        row("rvib.isReadOnly", () -> rvib.isReadOnly());
        row("rvib.hasArray", () -> rvib.hasArray());
        row("rvib.array", () -> rvib.array().length);
        row("rvib.put(0,1)", () -> rvib.put(0, 1));
        IntBuffer hib = IntBuffer.allocate(4);
        IntBuffer rib = hib.asReadOnlyBuffer();
        row("rib.getClass", () -> rib.getClass().getName());
        row("rib.hasArray", () -> rib.hasArray());
        row("rib.array", () -> rib.array().length);
        row("rib.arrayOffset", () -> rib.arrayOffset());

        // ---- the three BOUNDS exception classes ----------------------------
        System.out.println("== bounds: three classes ==");
        CharBuffer b4 = CharBuffer.allocate(4);
        row("cb.get(4) abs", () -> b4.get(4));
        row("cb.get(-1) abs", () -> b4.get(-1));
        row("cb.put(4,'z') abs", () -> b4.put(4, 'z'));
        row("cb.allocate(-1)", () -> CharBuffer.allocate(-1));
        CharBuffer empty = CharBuffer.allocate(4);
        empty.position(4);
        row("cb.get() rel at limit", () -> empty.get());
        row("cb.put('z') rel at limit", () -> empty.put('z'));
        CharBuffer four = CharBuffer.allocate(4);
        row("cb.get(char[8]) rel", () -> four.get(new char[8]));
        row("cb.put(char[8]) rel", () -> CharBuffer.allocate(4).put(new char[8]));
        row("cb.get(char[4],0,8) rel", () -> CharBuffer.allocate(4).get(new char[4], 0, 8));
        row("cb.get(char[4],-1,2) rel", () -> CharBuffer.allocate(4).get(new char[4], -1, 2));
        row("cb.get(0,char[4],0,8) abs", () -> CharBuffer.allocate(4).get(0, new char[4], 0, 8));
        row("cb.get(2,char[4],0,4) abs", () -> CharBuffer.allocate(4).get(2, new char[4], 0, 4));
        ByteBuffer bb4 = ByteBuffer.allocate(4);
        row("bb.get(byte[8]) rel", () -> bb4.get(new byte[8]));
        row("bb.put(byte[8]) rel", () -> ByteBuffer.allocate(4).put(new byte[8]));
        row("bb.get(2,byte[4],0,4) abs", () -> ByteBuffer.allocate(4).get(2, new byte[4], 0, 4));
        row("bb.get(0,byte[4],0,8) abs", () -> ByteBuffer.allocate(4).get(0, new byte[4], 0, 8));
        row("bb.getInt(2) abs", () -> ByteBuffer.allocate(4).getInt(2));
        row("bb.getInt() rel at 2", () -> ByteBuffer.allocate(4).position(2).getInt());
        row("bb.slice(1,9)", () -> ByteBuffer.allocate(4).slice(1, 9));
        row("cb.subSequence(0,9)", () -> CharBuffer.allocate(4).subSequence(0, 9));
        row("scb.subSequence(0,9)", () -> CharBuffer.wrap("abcd").subSequence(0, 9));
        row("cb.wrap(seq,1,9)", () -> CharBuffer.wrap("abcdef", 1, 9));
        row("cb.charAt(9)", () -> CharBuffer.allocate(4).charAt(9));
        row("cb.position(9)", () -> CharBuffer.allocate(4).position(9));
        row("cb.limit(9)", () -> CharBuffer.allocate(4).limit(9));
        row("cb.position(-1)", () -> CharBuffer.allocate(4).position(-1));
    }
}
