import java.nio.*;
import java.util.*;

/**
 * The typed VIEW buffers — `asIntBuffer` and its five siblings — over both
 * backings and both byte orders.
 *
 * `WORKER-4-NOTE-6` N2: after `W4Nio` (87 cases) and `W4Direct` (83), 53
 * buffer/channel §1.4 shadows were still unreached, and **30 of them are the
 * view classes**: `Buffer$2`, `CharBuffer`, `IntBuffer`, `LongBuffer`,
 * `ByteBufferAsCharBufferB`. Both earlier probes touch them at the edges — one
 * write and one read each.
 *
 * The view classes are the family where a drift is most likely and least
 * visible, because **the class name encodes both the backing and the byte
 * order** (`ByteBufferAsCharBufferB` is heap-ish/big-endian; there are `L`
 * variants, and direct buffers get their own set). Six widths x two backings x
 * two orders is twenty-four arms of one mechanism, and nothing had ever
 * compared them against each other, let alone against the oracle.
 *
 * The load-bearing assertion in each arm is WRITE-THROUGH: a value written via
 * the view must appear in the PARENT's bytes at the right offset, in the right
 * order. That is the one an offset or endianness error cannot survive, and it
 * is checked as hex rather than by reading the value back through the same
 * view (which would cancel a symmetric bug).
 */
public class W4Views {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    interface Thunk { Object call() throws Exception; }

    static void ckT(String tag, Thunk t) {
        try {
            ck(tag, t.call());
        } catch (Throwable e) {
            ck(tag, "threw:" + e.getClass().getName());
        }
    }

    static String st(Buffer b) {
        return b.position() + "/" + b.limit() + "/" + b.capacity();
    }

    /** The parent's raw bytes, independent of any view. */
    static String parentHex(ByteBuffer p) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < p.capacity(); i++) sb.append(String.format("%02x", p.get(i)));
        return sb.toString();
    }

    static ByteBuffer fresh(boolean direct, ByteOrder order, int cap) {
        ByteBuffer b = direct ? ByteBuffer.allocateDirect(cap) : ByteBuffer.allocate(cap);
        return b.order(order);
    }

    static void views(String tag, boolean direct, ByteOrder order) {
        // ---- CharBuffer ----
        ByteBuffer pc = fresh(direct, order, 16);
        CharBuffer cb = pc.asCharBuffer();
        ck(tag + ".char.state", st(cb));
        ck(tag + ".char.order", cb.order());
        ck(tag + ".char.isDirect", cb.isDirect());
        cb.put(0, 'A').put(1, 'é').put(2, '中');
        ck(tag + ".char.parentBytes", parentHex(pc));
        ck(tag + ".char.readBack", "" + cb.get(0) + cb.get(1) + cb.get(2));
        // `toString()` is UNDER TEST here, so it is read defensively: on this VM
        // a direct-backed char view answers "" (MEASURED), and a bare
        // `.substring(0, 3)` on that turns one wrong answer into a dead run
        // that hides every later case.
        ckT(tag + ".char.toString", () -> {
            String t = cb.toString();
            return t.length() >= 3 ? t.substring(0, 3) : "SHORT[" + t.length() + "]:" + t;
        });
        ckT(tag + ".char.oob", () -> cb.get(cb.capacity()));

        // ---- ShortBuffer ----
        ByteBuffer ps = fresh(direct, order, 16);
        ShortBuffer sb = ps.asShortBuffer();
        ck(tag + ".short.state", st(sb));
        sb.put(0, (short) 0x0102).put(1, (short) -2);
        ck(tag + ".short.parentBytes", parentHex(ps));
        ck(tag + ".short.readBack", sb.get(0) + "," + sb.get(1));

        // ---- IntBuffer ----
        ByteBuffer pi = fresh(direct, order, 16);
        IntBuffer ib = pi.asIntBuffer();
        ck(tag + ".int.state", st(ib));
        ib.put(0, 0x01020304).put(1, Integer.MIN_VALUE);
        ck(tag + ".int.parentBytes", parentHex(pi));
        ck(tag + ".int.readBack", ib.get(0) + "," + ib.get(1));
        // relative put must move the view's cursor and not the parent's
        ib.clear();
        ib.put(0x11223344);
        ck(tag + ".int.relative.viewState", st(ib));
        ck(tag + ".int.relative.parentState", st(pi));

        // ---- LongBuffer ----
        ByteBuffer pl = fresh(direct, order, 16);
        LongBuffer lb = pl.asLongBuffer();
        ck(tag + ".long.state", st(lb));
        lb.put(0, 0x0102030405060708L).put(1, -1L);
        ck(tag + ".long.parentBytes", parentHex(pl));
        ck(tag + ".long.readBack", lb.get(0) + "," + lb.get(1));

        // ---- FloatBuffer / DoubleBuffer ----
        ByteBuffer pf = fresh(direct, order, 16);
        FloatBuffer fb = pf.asFloatBuffer();
        fb.put(0, 1.0f).put(1, -2.5f);
        ck(tag + ".float.parentBytes", parentHex(pf));
        ck(tag + ".float.readBack", fb.get(0) + "," + fb.get(1));
        ByteBuffer pd = fresh(direct, order, 16);
        DoubleBuffer db = pd.asDoubleBuffer();
        db.put(0, 0.5d).put(1, Double.NEGATIVE_INFINITY);
        ck(tag + ".double.parentBytes", parentHex(pd));
        ck(tag + ".double.readBack", db.get(0) + "," + db.get(1));

        // ---- a view of a SLICE: the offset case -------------------------------
        ByteBuffer po = fresh(direct, order, 24);
        po.position(8);
        IntBuffer sliceView = po.slice().order(order).asIntBuffer();
        ck(tag + ".sliceView.state", st(sliceView));
        sliceView.put(0, 0x7f7f7f7f);
        ck(tag + ".sliceView.parentBytes", parentHex(po));

        // ---- a view of a READ-ONLY buffer ---------------------------------------
        ByteBuffer pro = fresh(direct, order, 8);
        pro.putInt(0, 0x0a0b0c0d);
        IntBuffer roView = pro.asReadOnlyBuffer().asIntBuffer();
        ck(tag + ".roView.isReadOnly", roView.isReadOnly());
        ck(tag + ".roView.get", String.format("%08x", roView.get(0)));
        ckT(tag + ".roView.put", () -> { roView.put(0, 1); return "no-throw"; });

        // ---- duplicate of a view keeps the store, not the cursor ------------------
        ByteBuffer pdup = fresh(direct, order, 16);
        IntBuffer v = pdup.asIntBuffer();
        v.put(0, 42);
        IntBuffer vd = v.duplicate();
        vd.position(2);
        ck(tag + ".viewDup.independentCursor", st(v) + " vs " + st(vd));
        ck(tag + ".viewDup.sharesStore", vd.get(0));
    }

    public static void main(String[] args) throws Exception {
        for (boolean direct : new boolean[] {false, true}) {
            for (ByteOrder order : new ByteOrder[] {ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN}) {
                String tag = (direct ? "direct" : "heap")
                        + "." + (order == ByteOrder.BIG_ENDIAN ? "be" : "le");
                // Each of the four arms is isolated. Twenty-four view variants
                // are being compared and the interesting output is the SET of
                // diffs; an arm that dies must cost its own cases and not the
                // three arms after it.
                try {
                    views(tag, direct, order);
                } catch (Throwable t) {
                    ck(tag + ".ARM-ABORTED", t.getClass().getName());
                }
            }
        }

        // ---- wrapped standalone views, which have no ByteBuffer parent ----------
        ck("wrap.int.state", st(IntBuffer.wrap(new int[] {1, 2, 3})));
        ck("wrap.int.hasArray", IntBuffer.wrap(new int[] {1}).hasArray());
        ck("wrap.char.toString", CharBuffer.wrap("abcdef", 1, 4).toString());
        ck("wrap.char.subSequence", CharBuffer.wrap("abcdef").subSequence(2, 4).toString());
        ck("wrap.int.allocate", st(IntBuffer.allocate(4)));
        ck("wrap.char.compareTo",
                Integer.signum(CharBuffer.wrap("abc").compareTo(CharBuffer.wrap("abd"))));
        ck("wrap.int.equals",
                IntBuffer.wrap(new int[] {1, 2}).equals(IntBuffer.wrap(new int[] {1, 2})));

        System.out.println("PASS W4Views");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
