import java.nio.*;
import java.util.*;

/** L4 -- `java.nio.ByteBuffer`, asked at its CONTRACT EDGES.
 *
 *  A buffer is four invariants (`0 <= mark <= position <= limit <= capacity`)
 *  and about thirty methods that must each preserve them or refuse. The
 *  refusals are what a from-memory implementation gets wrong, and they are
 *  SPECIFIC types, not one type: `IllegalArgumentException` for an out-of-range
 *  position or limit, `InvalidMarkException` for a reset with no mark,
 *  `BufferUnderflowException` / `BufferOverflowException` for relative access
 *  past the limit, `IndexOutOfBoundsException` for ABSOLUTE access past it, and
 *  `ReadOnlyBufferException` for every mutator on a read-only view. A shim that
 *  answers `IllegalArgumentException` to all six looks right in a `try/catch
 *  (Exception)` and is wrong to every real caller.
 *
 *  Each block is asked of THREE backings -- heap, direct, and a wrapped array
 *  with a non-zero offset -- because `hasArray`, `array`, `arrayOffset` and
 *  `slice` differ across exactly those and a probe on one backing reports the
 *  family clean.
 *
 *  Nothing here prints an ADDRESS: `buffer-address-is-nonzero-for-heap-buffers`
 *  records that the two VMs may legitimately disagree there.
 */
public class L4ByteBufferSweep {
    static int rows = 0;
    static void p(String tag, Object v) {
        rows++;
        System.out.println(tag + " |" + String.valueOf(v) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        rows++;
        try { r.run(); System.out.println(tag + " |no-throw|"); }
        catch (Throwable e) { System.out.println(tag + " |THREW " + e.getClass().getName() + "|"); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    static String st(Buffer b) {
        return "p=" + b.position() + " l=" + b.limit() + " c=" + b.capacity() + " r=" + b.remaining();
    }
    static String hex(byte[] a) {
        if (a == null) return "null";
        StringBuilder s = new StringBuilder();
        for (byte x : a) s.append(String.format("%02x", x));
        return s.toString();
    }
    static String dump(ByteBuffer b) {
        ByteBuffer d = b.duplicate();
        d.clear();
        byte[] a = new byte[d.capacity()];
        d.get(a);
        return hex(a);
    }

    interface Maker { ByteBuffer make(); }
    static final LinkedHashMap<String, Maker> BACKINGS = new LinkedHashMap<>();
    static {
        BACKINGS.put("heap", () -> ByteBuffer.allocate(16));
        BACKINGS.put("direct", () -> ByteBuffer.allocateDirect(16));
        BACKINGS.put("wrapped-off", () -> ByteBuffer.wrap(new byte[24], 4, 16).slice());
    }

    // ------------------------------------------------------------ invariants

    static void invariants(String k, Maker m) {
        ByteBuffer b = m.make();
        p(k + " fresh", st(b));
        p(k + " isDirect", b.isDirect());
        p(k + " isReadOnly", b.isReadOnly());
        p(k + " hasArray", b.hasArray());
        p(k + " order", b.order());
        p(k + " hasRemaining", b.hasRemaining());

        b.position(4);
        p(k + " after position(4)", st(b));
        b.limit(8);
        p(k + " after limit(8)", st(b));
        // limit BELOW position drags position down with it.
        b.limit(2);
        p(k + " after limit(2) below position", st(b));
        b.clear();
        p(k + " after clear", st(b));
        b.position(6);
        b.flip();
        p(k + " after flip from 6", st(b));
        b.rewind();
        p(k + " after rewind", st(b));

        // Out-of-range position / limit are IllegalArgumentException.
        ByteBuffer e = m.make();
        t(k + " position(-1)", () -> e.position(-1));
        t(k + " position(cap+1)", () -> e.position(17));
        t(k + " position(cap)", () -> e.position(16));
        t(k + " limit(-1)", () -> e.limit(-1));
        t(k + " limit(cap+1)", () -> e.limit(17));
        t(k + " limit(cap)", () -> e.limit(16));
        e.clear();
        e.limit(8);
        t(k + " position past limit", () -> e.position(9));
        p(k + " state after refusals", st(e));

        // mark / reset. A reset with no mark is InvalidMarkException, NOT
        // IllegalStateException and not IllegalArgumentException.
        ByteBuffer mk = m.make();
        t(k + " reset with no mark", () -> mk.reset());
        mk.position(4);
        mk.mark();
        mk.position(8);
        mk.reset();
        p(k + " after mark(4)/reset", st(mk));
        // A limit set BELOW the mark discards it.
        mk.mark();
        mk.limit(2);
        t(k + " reset after limit below mark", () -> mk.reset());
        mk.clear();
        t(k + " reset after clear discards mark", () -> mk.reset());
        mk.position(4);
        mk.mark();
        mk.rewind();
        t(k + " reset after rewind discards mark", () -> mk.reset());
        mk.position(4);
        mk.mark();
        mk.flip();
        t(k + " reset after flip discards mark", () -> mk.reset());
    }

    // -------------------------------------------------------------- transfer

    static void transfer(String k, Maker m) {
        ByteBuffer b = m.make();
        b.put((byte) 1).put((byte) 2).put((byte) 3);
        p(k + " after 3 relative puts", st(b));
        b.flip();
        p(k + " get", b.get());
        p(k + " get(1) absolute", b.get(1));
        p(k + " absolute get does not move", st(b));
        byte[] into = new byte[2];
        b.get(into);
        p(k + " get(byte[2])", hex(into));
        p(k + " state after bulk get", st(b));
        t(k + " get past limit", () -> b.get());
        t(k + " get(byte[4]) past limit", () -> b.get(new byte[4]));
        p(k + " state unchanged after underflow", st(b));

        // Absolute access is IndexOutOfBounds, not BufferUnderflow, and it is
        // bounded by the CAPACITY-independent limit rules: absolute get is
        // checked against the LIMIT.
        ByteBuffer c = m.make();
        c.limit(8);
        t(k + " absolute get(-1)", () -> c.get(-1));
        t(k + " absolute get(limit)", () -> c.get(8));
        t(k + " absolute get(cap-1) past limit", () -> c.get(15));
        t(k + " absolute put(limit)", () -> c.put(8, (byte) 1));
        t(k + " absolute getInt(limit-3)", () -> c.getInt(5));
        t(k + " absolute getInt(limit-4) ok", () -> c.getInt(4));

        // Relative overflow is BufferOverflowException.
        ByteBuffer o = m.make();
        o.position(14);
        t(k + " putInt with 2 left", () -> o.putInt(1));
        t(k + " put(byte[4]) with 2 left", () -> o.put(new byte[4]));
        p(k + " state after overflow", st(o));
        t(k + " put 2 then 1 more", () -> { o.put((byte) 1); o.put((byte) 2); o.put((byte) 3); });

        // The array-with-offset forms.
        ByteBuffer a = m.make();
        byte[] src = {9, 8, 7, 6};
        t(k + " put(src,-1,2)", () -> a.put(src, -1, 2));
        t(k + " put(src,0,-1)", () -> a.put(src, 0, -1));
        t(k + " put(src,3,2)", () -> a.put(src, 3, 2));
        t(k + " put(src,4,0)", () -> a.put(src, 4, 0));
        t(k + " put(src,1,MAX)", () -> a.put(src, 1, Integer.MAX_VALUE));
        t(k + " put(null,0,1)", () -> a.put((byte[]) null, 0, 1));
        t(k + " get(null,0,1)", () -> a.get((byte[]) null, 0, 1));
        p(k + " state after arg refusals", st(a));
        a.put(src, 1, 2);
        p(k + " put(src,1,2)", st(a));
        a.flip();
        byte[] out = new byte[4];
        a.get(out, 1, 2);
        p(k + " get(out,1,2)", hex(out));

        // put(ByteBuffer): the source is drained, self-put is refused.
        ByteBuffer dst = m.make();
        ByteBuffer s2 = ByteBuffer.wrap(new byte[]{4, 5});
        dst.put(s2);
        p(k + " put(ByteBuffer)", st(dst));
        p(k + " source drained", st(s2));
        t(k + " put(self)", () -> dst.put(dst));
        ByteBuffer big = ByteBuffer.allocate(64);
        t(k + " put(bigger buffer)", () -> m.make().put(big));
    }

    // ------------------------------------------------------------ typed I/O

    static void typed(String k, Maker m) {
        ByteBuffer b = m.make();
        b.putInt(0x01020304);
        b.putShort((short) 0x0506);
        b.putChar('A');
        b.put((byte) 0x77);
        b.putFloat(1.5f);
        b.flip();
        p(k + " big-endian bytes", dump(b));
        p(k + " getInt", Integer.toHexString(b.getInt()));
        p(k + " getShort", Integer.toHexString(b.getShort() & 0xFFFF));
        p(k + " getChar", b.getChar());
        p(k + " get", b.get());
        p(k + " getFloat", b.getFloat());
        p(k + " state after typed reads", st(b));

        ByteBuffer le = m.make();
        le.order(ByteOrder.LITTLE_ENDIAN);
        p(k + " order after set", le.order());
        le.putInt(0x01020304);
        le.putLong(0x0102030405060708L);
        le.putFloat(2.5f);
        le.flip();
        p(k + " little-endian bytes", dump(le));
        p(k + " le getInt", Integer.toHexString(le.getInt()));
        p(k + " le getLong", Long.toHexString(le.getLong()));
        p(k + " le getFloat", le.getFloat());
        // A duplicate does NOT inherit the byte order in Java 8; since 9 it
        // does. Ask, rather than assume.
        p(k + " duplicate inherits order", le.duplicate().order());
        p(k + " slice inherits order", le.slice().order());
        p(k + " asReadOnly inherits order", le.asReadOnlyBuffer().order());
        t(k + " order(null)", () -> m.make().order(null));

        // Absolute typed accessors do not move the position.
        ByteBuffer ab = m.make();
        ab.putInt(0, 0x0A0B0C0D);
        ab.putLong(4, 1L);
        ab.putShort(12, (short) 3);
        ab.putChar(14, 'q');
        p(k + " absolute writes leave position", st(ab));
        p(k + " absolute getInt(0)", Integer.toHexString(ab.getInt(0)));
        p(k + " absolute getLong(4)", ab.getLong(4));
        p(k + " absolute getShort(12)", ab.getShort(12));
        p(k + " absolute getChar(14)", ab.getChar(14));
        t(k + " absolute getLong(9) past end", () -> ab.getLong(9));
        t(k + " absolute putLong(-1)", () -> ab.putLong(-1, 1L));
    }

    // ------------------------------------------------------------ views

    static void views(String k, Maker m) {
        ByteBuffer b = m.make();
        for (int i = 0; i < 16; i++) b.put(i, (byte) (i + 1));
        b.position(4);
        b.limit(12);

        ByteBuffer sl = b.slice();
        p(k + " slice state", st(sl));
        p(k + " slice first byte", sl.get(0));
        p(k + " slice hasArray", sl.hasArray());
        p(k + " slice arrayOffset", sl.hasArray() ? sl.arrayOffset() : -1);
        sl.put(0, (byte) 99);
        p(k + " slice write is shared", b.get(4));
        p(k + " parent unchanged by slice", st(b));

        ByteBuffer du = b.duplicate();
        p(k + " duplicate state", st(du));
        du.position(6);
        p(k + " duplicate position is independent", st(b));
        du.put(6, (byte) 88);
        p(k + " duplicate write is shared", b.get(6));

        ByteBuffer sl2 = b.slice(2, 4);
        p(k + " slice(2,4) state", st(sl2));
        p(k + " slice(2,4) first byte", sl2.get(0));
        t(k + " slice(-1,2)", () -> b.slice(-1, 2));
        t(k + " slice(0,cap+1)", () -> b.slice(0, 17));

        // A read-only view refuses every mutator, with ReadOnlyBufferException.
        ByteBuffer ro = b.asReadOnlyBuffer();
        p(k + " readOnly isReadOnly", ro.isReadOnly());
        p(k + " readOnly hasArray", ro.hasArray());
        p(k + " readOnly state", st(ro));
        p(k + " readOnly can read", ro.get(0));
        t(k + " readOnly put", () -> ro.put((byte) 1));
        t(k + " readOnly put absolute", () -> ro.put(0, (byte) 1));
        t(k + " readOnly putInt", () -> ro.putInt(1));
        t(k + " readOnly put(byte[])", () -> ro.put(new byte[]{1}));
        t(k + " readOnly compact", () -> ro.compact());
        t(k + " readOnly array", () -> ro.array());
        t(k + " readOnly arrayOffset", () -> ro.arrayOffset());
        p(k + " readOnly slice is readOnly", ro.slice().isReadOnly());
        p(k + " readOnly duplicate is readOnly", ro.duplicate().isReadOnly());
        p(k + " readOnly asReadOnly is readOnly", ro.asReadOnlyBuffer().isReadOnly());
        // Read-only is CONTAGIOUS but not reversible: there is no way back.
        p(k + " readOnly of a readOnly still readOnly", ro.asReadOnlyBuffer().asReadOnlyBuffer().isReadOnly());
        p(k + " parent still writable", b.isReadOnly());

        // array() / arrayOffset() on a buffer with no accessible array.
        ByteBuffer d = ByteBuffer.allocateDirect(8);
        p("direct hasArray", d.hasArray());
        t("direct array()", () -> d.array());
        t("direct arrayOffset()", () -> d.arrayOffset());

        // compact moves the remaining bytes to the front.
        ByteBuffer cp = m.make();
        for (int i = 0; i < 16; i++) cp.put((byte) (i + 1));
        cp.flip();
        cp.position(12);
        cp.compact();
        p(k + " compact state", st(cp));
        p(k + " compact moved bytes", dump(cp).substring(0, 8));

        // Typed views over the byte buffer.
        ByteBuffer v = m.make();
        v.position(2);
        IntBuffer ib = v.asIntBuffer();
        p(k + " asIntBuffer state", st(ib));
        ib.put(0, 0x11223344);
        p(k + " intbuffer write reaches bytes", Integer.toHexString(v.getInt(2)));
        CharBuffer cb = v.asCharBuffer();
        p(k + " asCharBuffer capacity", cb.capacity());
        p(k + " asLongBuffer capacity", v.asLongBuffer().capacity());
        p(k + " asShortBuffer capacity", v.asShortBuffer().capacity());
        p(k + " asDoubleBuffer capacity", v.asDoubleBuffer().capacity());
        p(k + " asFloatBuffer capacity", v.asFloatBuffer().capacity());
    }

    // ------------------------------------------------------- value semantics

    static void valueSemantics() {
        // equals / compareTo / hashCode are defined over the REMAINING
        // elements only -- two buffers with different capacities and positions
        // are equal when their remainders match.
        ByteBuffer a = ByteBuffer.wrap(new byte[]{1, 2, 3, 4, 5});
        ByteBuffer b = ByteBuffer.wrap(new byte[]{9, 9, 3, 4, 5});
        a.position(2);
        b.position(2);
        p("equal remainders", a.equals(b));
        p("hashCode agrees", a.hashCode() == b.hashCode());
        p("compareTo equal", a.compareTo(b));
        p("mismatch", a.mismatch(b));
        b.position(1);
        p("different remainders", a.equals(b));
        p("compareTo sign", Integer.signum(a.compareTo(b)));
        p("equals other type", a.equals("x"));
        p("equals null", a.equals(null));
        p("empty buffers equal", ByteBuffer.allocate(0).equals(ByteBuffer.allocate(4).position(4)));
        p("mismatch identical", ByteBuffer.wrap(new byte[]{1}).mismatch(ByteBuffer.wrap(new byte[]{1})));
        p("mismatch prefix", ByteBuffer.wrap(new byte[]{1}).mismatch(ByteBuffer.wrap(new byte[]{1, 2})));
        // A direct and a heap buffer with the same contents ARE equal.
        ByteBuffer h = ByteBuffer.wrap(new byte[]{1, 2});
        ByteBuffer d = ByteBuffer.allocateDirect(2);
        d.put((byte) 1).put((byte) 2).flip();
        p("heap equals direct", h.equals(d));
        p("heap hashCode equals direct", h.hashCode() == d.hashCode());
        // A read-only view equals its source.
        p("readOnly equals source", h.equals(h.asReadOnlyBuffer()));
    }

    // ------------------------------------------------------------ allocation

    static void allocation() {
        t("allocate(-1)", () -> ByteBuffer.allocate(-1));
        t("allocate(0)", () -> ByteBuffer.allocate(0));
        t("allocateDirect(-1)", () -> ByteBuffer.allocateDirect(-1));
        t("allocateDirect(0)", () -> ByteBuffer.allocateDirect(0));
        p("allocate(0) state", st(ByteBuffer.allocate(0)));
        p("allocate(0) hasArray", ByteBuffer.allocate(0).hasArray());
        t("wrap(null)", () -> ByteBuffer.wrap((byte[]) null));
        t("wrap(null,0,0)", () -> ByteBuffer.wrap(null, 0, 0));
        t("wrap(b,-1,2)", () -> ByteBuffer.wrap(new byte[4], -1, 2));
        t("wrap(b,0,5)", () -> ByteBuffer.wrap(new byte[4], 0, 5));
        t("wrap(b,4,0)", () -> ByteBuffer.wrap(new byte[4], 4, 0));
        t("wrap(b,1,MAX)", () -> ByteBuffer.wrap(new byte[4], 1, Integer.MAX_VALUE));
        ByteBuffer w = ByteBuffer.wrap(new byte[8], 2, 4);
        p("wrap(b,2,4) state", st(w));
        p("wrap(b,2,4) arrayOffset", w.arrayOffset());
        p("wrap(b,2,4) hasArray", w.hasArray());
        p("wrap(b,2,4) array length", w.array().length);
        w.clear();
        p("wrap clear goes to 0..cap", st(w));
        // wrap shares the array: a write through the buffer is visible in it.
        byte[] backing = new byte[4];
        ByteBuffer sh = ByteBuffer.wrap(backing);
        sh.put(0, (byte) 5);
        p("wrap shares the array", backing[0]);
        p("array() is the same object", sh.array() == backing);
        // The whole-array wrap has offset 0 and capacity == length.
        p("wrap(b) state", st(ByteBuffer.wrap(new byte[3])));
        p("wrap(b) arrayOffset", ByteBuffer.wrap(new byte[3]).arrayOffset());
    }

    public static void main(String[] a) {
        for (Map.Entry<String, Maker> e : BACKINGS.entrySet()) {
            invariants(e.getKey(), e.getValue());
            transfer(e.getKey(), e.getValue());
            typed(e.getKey(), e.getValue());
            views(e.getKey(), e.getValue());
        }
        valueSemantics();
        allocation();
        System.out.println("rows " + rows);
        System.out.println("DONE L4ByteBufferSweep");
    }
}
