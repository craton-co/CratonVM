import java.nio.*;
import java.util.*;

/** L4 — the FIVE typed buffers and `CharBuffer`, at their contract edges.
 *
 *  `L4ByteBufferSweep` was 0-diff over 404 rows, which says nothing about
 *  these: `IntBuffer`, `LongBuffer`, `ShortBuffer`, `FloatBuffer`,
 *  `DoubleBuffer` and `CharBuffer` carry their OWN registrations — 5 rows
 *  each in the census, and not one of them had been reached by any probe in
 *  the tree.
 *
 *  Each type is asked of THREE backings, because the three disagree on
 *  exactly the rows this probe exists for:
 *
 *    allocate(n)              hasArray true,  writable
 *    ByteBuffer.asXBuffer()   hasArray FALSE, writable  <- `array()` must refuse
 *    asReadOnlyBuffer()       hasArray FALSE, refuses every mutator
 *
 *  The view-over-a-ByteBuffer backing is the one a happy-path probe misses:
 *  it is what `ByteBuffer.asIntBuffer()` returns, it has no accessible array,
 *  and its `array()`/`arrayOffset()` are `UnsupportedOperationException` while
 *  a read-only buffer's are `ReadOnlyBufferException`. Two different refusals
 *  for the same-looking call.
 *
 *  Hygiene: no addresses, no identity hashes, and `toString()` is asked
 *  deliberately (it names the implementation class, which is a real identity
 *  claim) rather than by accident.
 */
public class L4TypedBufferSweep {
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
    /** `len` bytes of `a` from `off`, as lowercase hex. The byte pattern is the
     *  only thing that can distinguish a view that is wrong about its order
     *  from one that is right, because a wrong view still round-trips. */
    static String hex(byte[] a, int off, int len) {
        StringBuilder b = new StringBuilder();
        for (int i = off; i < off + len; i++) b.append(String.format("%02x", a[i]));
        return b.toString();
    }

    static String st(Buffer b) {
        return "p=" + b.position() + " l=" + b.limit() + " c=" + b.capacity()
             + " r=" + b.remaining() + " hr=" + b.hasRemaining();
    }

    /** The shape every `java.nio.Buffer` subclass shares, asked through the
     *  BASE type so the `java/nio/Buffer` registrations are the ones reached. */
    static void base(String k, Buffer b) {
        p(k + " fresh", st(b));
        p(k + " isReadOnly", b.isReadOnly());
        p(k + " isDirect", b.isDirect());
        p(k + " hasArray", b.hasArray());
        b.position(1);
        p(k + " after position(1)", st(b));
        b.limit(3);
        p(k + " after limit(3)", st(b));
        b.mark();
        b.position(2);
        b.reset();
        p(k + " after mark(1)/reset", st(b));
        b.clear();
        p(k + " after clear", st(b));
        b.position(2);
        b.flip();
        p(k + " after flip", st(b));
        b.rewind();
        p(k + " after rewind", st(b));
        t(k + " position(-1)", () -> b.position(-1));
        t(k + " position(cap+1)", () -> b.position(b.capacity() + 1));
        t(k + " limit(cap+1)", () -> b.limit(b.capacity() + 1));
        b.clear();
        t(k + " reset with no mark", () -> b.reset());
        // `array()` and `arrayOffset()` refuse for two DIFFERENT reasons:
        // UnsupportedOperationException with no accessible array,
        // ReadOnlyBufferException on a read-only one. Both are asked.
        t(k + " array()", () -> b.array());
        t(k + " arrayOffset()", () -> b.arrayOffset());
        p(k + " toString", b.toString());
    }

    // ------------------------------------------------------------- per type

    static void ints(String k, IntBuffer b, boolean writable) {
        base(k, b);
        b.clear();
        if (writable) {
            b.put(new int[]{1, 2, 3, 4}, 0, 4);
            b.flip();
            p(k + " put/flip", st(b));
            p(k + " get()", b.get());
            p(k + " get(2)", b.get(2));
            int[] out = new int[4];
            b.get(out, 1, 2);
            p(k + " get(out,1,2)", Arrays.toString(out));
            t(k + " put(src,-1,2)", () -> b.put(new int[]{1, 2}, -1, 2));
            t(k + " put(src,0,-1)", () -> b.put(new int[]{1, 2}, 0, -1));
            t(k + " put(src,1,2)", () -> b.put(new int[]{1, 2}, 1, 2));
            t(k + " put(null,0,1)", () -> b.put((int[]) null, 0, 1));
            t(k + " get(null,0,1)", () -> b.get((int[]) null, 0, 1));
            b.clear();
            t(k + " put past capacity", () -> b.put(new int[b.capacity() + 1]));
            b.clear();
            b.position(b.limit());
            t(k + " get at limit", () -> b.get());
            t(k + " put at limit", () -> b.put(1));
        } else {
            t(k + " put refused", () -> b.put(0));
            t(k + " put(int[]) refused", () -> b.put(new int[]{1}));
            t(k + " put(0,1) refused", () -> b.put(0, 1));
            t(k + " compact refused", () -> b.compact());
        }
        b.clear();
        p(k + " slice", st(b.slice()));
        p(k + " duplicate", st(b.duplicate()));
        p(k + " asReadOnly isReadOnly", b.asReadOnlyBuffer().isReadOnly());
        p(k + " order", b.order());
        p(k + " equals self-dup", b.equals(b.duplicate()));
        p(k + " hashCode agrees", b.hashCode() == b.duplicate().hashCode());
        p(k + " compareTo self-dup", b.compareTo(b.duplicate()));
        p(k + " mismatch self-dup", b.mismatch(b.duplicate()));
        p(k + " equals other type", b.equals("x"));
    }

    static void longs(String k, LongBuffer b, boolean writable) {
        base(k, b);
        b.clear();
        if (writable) {
            b.put(new long[]{1, 2, 3, 4}, 0, 4);
            b.flip();
            p(k + " get()", b.get());
            long[] out = new long[4];
            b.get(out, 1, 2);
            p(k + " get(out,1,2)", Arrays.toString(out));
            t(k + " put(null,0,1)", () -> b.put((long[]) null, 0, 1));
            t(k + " put(src,1,2)", () -> b.put(new long[]{1, 2}, 1, 2));
        } else {
            t(k + " put refused", () -> b.put(0L));
        }
        b.clear();
        p(k + " equals self-dup", b.equals(b.duplicate()));
        p(k + " compareTo self-dup", b.compareTo(b.duplicate()));
    }

    static void shorts(String k, ShortBuffer b, boolean writable) {
        base(k, b);
        b.clear();
        if (writable) {
            b.put(new short[]{1, 2, 3, 4}, 0, 4);
            b.flip();
            p(k + " get()", b.get());
            short[] out = new short[4];
            b.get(out, 1, 2);
            p(k + " get(out,1,2)", Arrays.toString(out));
            t(k + " put(null,0,1)", () -> b.put((short[]) null, 0, 1));
            t(k + " put(src,1,2)", () -> b.put(new short[]{1, 2}, 1, 2));
        } else {
            t(k + " put refused", () -> b.put((short) 0));
        }
        b.clear();
        p(k + " equals self-dup", b.equals(b.duplicate()));
        p(k + " compareTo self-dup", b.compareTo(b.duplicate()));
    }

    static void floats(String k, FloatBuffer b, boolean writable) {
        base(k, b);
        b.clear();
        if (writable) {
            b.put(new float[]{1.5f, 2.5f, 3.5f, 4.5f}, 0, 4);
            b.flip();
            p(k + " get()", b.get());
            float[] out = new float[4];
            b.get(out, 1, 2);
            p(k + " get(out,1,2)", Arrays.toString(out));
            t(k + " put(null,0,1)", () -> b.put((float[]) null, 0, 1));
            // NaN is the row a float buffer's equals gets wrong: the JDK
            // compares with `Float.compare` semantics, under which NaN EQUALS
            // NaN and -0.0 does NOT equal 0.0 — the opposite of `==` on both.
            FloatBuffer n1 = FloatBuffer.allocate(1);
            FloatBuffer n2 = FloatBuffer.allocate(1);
            n1.put(0, Float.NaN);
            n2.put(0, Float.NaN);
            p(k + " NaN equals NaN", n1.equals(n2));
            n1.put(0, 0.0f);
            n2.put(0, -0.0f);
            p(k + " 0.0 equals -0.0", n1.equals(n2));
            p(k + " compareTo 0.0 vs -0.0", Integer.signum(n1.compareTo(n2)));
        } else {
            t(k + " put refused", () -> b.put(0f));
        }
        b.clear();
        p(k + " equals self-dup", b.equals(b.duplicate()));
    }

    static void doubles(String k, DoubleBuffer b, boolean writable) {
        base(k, b);
        b.clear();
        if (writable) {
            b.put(new double[]{1.5, 2.5, 3.5, 4.5}, 0, 4);
            b.flip();
            p(k + " get()", b.get());
            double[] out = new double[4];
            b.get(out, 1, 2);
            p(k + " get(out,1,2)", Arrays.toString(out));
            t(k + " put(null,0,1)", () -> b.put((double[]) null, 0, 1));
            DoubleBuffer n1 = DoubleBuffer.allocate(1);
            DoubleBuffer n2 = DoubleBuffer.allocate(1);
            n1.put(0, Double.NaN);
            n2.put(0, Double.NaN);
            p(k + " NaN equals NaN", n1.equals(n2));
            n1.put(0, 0.0);
            n2.put(0, -0.0);
            p(k + " 0.0 equals -0.0", n1.equals(n2));
        } else {
            t(k + " put refused", () -> b.put(0d));
        }
        b.clear();
        p(k + " equals self-dup", b.equals(b.duplicate()));
    }

    static void chars(String k, CharBuffer b, boolean writable) {
        base(k, b);
        b.clear();
        if (writable) {
            b.put(new char[]{'a', 'b', 'c', 'd'}, 0, 4);
            b.flip();
            p(k + " get()", b.get());
            char[] out = new char[4];
            b.get(out, 1, 2);
            p(k + " get(out,1,2)", new String(out).replace('\0', '.'));
            t(k + " put(null,0,1)", () -> b.put((char[]) null, 0, 1));
        } else {
            t(k + " put refused", () -> b.put('x'));
        }
        b.clear();
        // The ABSOLUTE `get(int)` and the RELATIVE `charAt(int)` are two
        // contracts on one class, and asking them at position 0 cannot tell
        // them apart — which is how a shared implementation goes unnoticed.
        // Ask both at a NON-ZERO position.
        b.position(1);
        t(k + " get(0) absolute at position 1", () -> { char c = b.get(0); p(k + " get(0)=", c); });
        t(k + " charAt(0) relative at position 1", () -> { char c = b.charAt(0); p(k + " charAt(0)=", c); });
        t(k + " get(limit) absolute", () -> b.get(b.limit()));
        t(k + " get(-1) absolute", () -> b.get(-1));
        p(k + " position unmoved by absolute get", st(b));
        b.clear();
        // CharBuffer is a CharSequence, and that half has its own contract.
        p(k + " charAt(0)", b.charAt(0));
        t(k + " charAt(-1)", () -> b.charAt(-1));
        t(k + " charAt(remaining)", () -> b.charAt(b.remaining()));
        p(k + " length", b.length());
        p(k + " subSequence(1,3)", b.subSequence(1, 3).toString());
        t(k + " subSequence(3,1)", () -> b.subSequence(3, 1));
        t(k + " subSequence(0,99)", () -> b.subSequence(0, 99));
        p(k + " toString is the remainder", b.toString());
        b.position(2);
        p(k + " toString after position(2)", b.toString());
        p(k + " length after position(2)", b.length());
        b.clear();
    }

    public static void main(String[] a) {
        // 1. allocate(): has an accessible array, writable.
        ints("int/alloc", IntBuffer.allocate(4), true);
        longs("long/alloc", LongBuffer.allocate(4), true);
        shorts("short/alloc", ShortBuffer.allocate(4), true);
        floats("float/alloc", FloatBuffer.allocate(4), true);
        doubles("double/alloc", DoubleBuffer.allocate(4), true);
        chars("char/alloc", CharBuffer.allocate(4), true);

        // 2. a VIEW over a ByteBuffer: writable, but NO accessible array.
        ints("int/view", ByteBuffer.allocate(16).asIntBuffer(), true);
        longs("long/view", ByteBuffer.allocate(32).asLongBuffer(), true);
        shorts("short/view", ByteBuffer.allocate(8).asShortBuffer(), true);
        floats("float/view", ByteBuffer.allocate(16).asFloatBuffer(), true);
        doubles("double/view", ByteBuffer.allocate(32).asDoubleBuffer(), true);
        chars("char/view", ByteBuffer.allocate(8).asCharBuffer(), true);

        // 3. read-only: refuses every mutator, for a DIFFERENT reason.
        ints("int/ro", IntBuffer.allocate(4).asReadOnlyBuffer(), false);
        longs("long/ro", LongBuffer.allocate(4).asReadOnlyBuffer(), false);
        shorts("short/ro", ShortBuffer.allocate(4).asReadOnlyBuffer(), false);
        floats("float/ro", FloatBuffer.allocate(4).asReadOnlyBuffer(), false);
        doubles("double/ro", DoubleBuffer.allocate(4).asReadOnlyBuffer(), false);
        chars("char/ro", CharBuffer.allocate(4).asReadOnlyBuffer(), false);

        // 4. the allocators' own refusals, and `CharBuffer.wrap`'s two forms.
        t("IntBuffer.allocate(-1)", () -> IntBuffer.allocate(-1));
        t("LongBuffer.allocate(-1)", () -> LongBuffer.allocate(-1));
        t("ShortBuffer.allocate(-1)", () -> ShortBuffer.allocate(-1));
        t("FloatBuffer.allocate(-1)", () -> FloatBuffer.allocate(-1));
        t("DoubleBuffer.allocate(-1)", () -> DoubleBuffer.allocate(-1));
        t("CharBuffer.allocate(-1)", () -> CharBuffer.allocate(-1));
        p("IntBuffer.allocate(0)", st(IntBuffer.allocate(0)));
        t("IntBuffer.wrap(null)", () -> IntBuffer.wrap((int[]) null));
        t("IntBuffer.wrap(a,-1,2)", () -> IntBuffer.wrap(new int[4], -1, 2));
        t("IntBuffer.wrap(a,0,5)", () -> IntBuffer.wrap(new int[4], 0, 5));
        p("IntBuffer.wrap(a,1,2)", st(IntBuffer.wrap(new int[4], 1, 2)));
        p("IntBuffer.wrap arrayOffset", IntBuffer.wrap(new int[4], 1, 2).arrayOffset());
        p("CharBuffer.wrap(CharSequence)", st(CharBuffer.wrap("hello")));
        p("CharBuffer.wrap(cs) isReadOnly", CharBuffer.wrap("hello").isReadOnly());
        p("CharBuffer.wrap(cs,1,3)", CharBuffer.wrap("hello", 1, 3).toString());
        t("CharBuffer.wrap((CharSequence)null)", () -> CharBuffer.wrap((CharSequence) null));
        t("CharBuffer.wrap(cs,3,1)", () -> CharBuffer.wrap("hello", 3, 1));

        // 5. a wrapped array is SHARED, and a view writes through to its bytes.
        int[] backing = new int[2];
        IntBuffer w = IntBuffer.wrap(backing);
        w.put(0, 7);
        p("wrap shares the array", backing[0]);
        p("array() is the same object", w.array() == backing);
        ByteBuffer bb = ByteBuffer.allocate(8);
        IntBuffer v = bb.asIntBuffer();
        v.put(0, 0x01020304);
        p("view writes through to the bytes", Integer.toHexString(bb.getInt(0)));
        p("view capacity is bytes/4", v.capacity());
        bb.order(ByteOrder.LITTLE_ENDIAN);
        p("view takes the order at creation", bb.asIntBuffer().order());

        // 6. THE BYTE-ORDER ARM.
        //
        // WORKER-4-NOTE-6 N2 asked for the view classes "at every width, over
        // BOTH backings and BOTH byte orders", and this probe had ONE row of
        // one order — the line directly above. The order is not a display
        // setting for these classes: it is part of the IMPLEMENTATION CLASS's
        // name (`ByteBufferAsCharBufferB` versus `...L`), so a big-endian-only
        // sweep exercises one of every pair and reports it as the family.
        // Little-endian is also the host order on every platform this VM
        // supports, so it is the arm an application actually gets from
        // `order(nativeOrder())`.
        for (ByteOrder o : new ByteOrder[] { ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN }) {
            String k = (o == ByteOrder.BIG_ENDIAN) ? "BE" : "LE";
            ints("int/view/" + k, ByteBuffer.allocate(16).order(o).asIntBuffer(), true);
            longs("long/view/" + k, ByteBuffer.allocate(32).order(o).asLongBuffer(), true);
            shorts("short/view/" + k, ByteBuffer.allocate(8).order(o).asShortBuffer(), true);
            floats("float/view/" + k, ByteBuffer.allocate(16).order(o).asFloatBuffer(), true);
            doubles("double/view/" + k, ByteBuffer.allocate(32).order(o).asDoubleBuffer(), true);
            chars("char/view/" + k, ByteBuffer.allocate(8).order(o).asCharBuffer(), true);
            // Read-only views of each order: a second implementation class per
            // pair (`...RB` / `...RL`), and the one that must refuse.
            ints("int/view/ro/" + k, ByteBuffer.allocate(16).order(o).asIntBuffer().asReadOnlyBuffer(), false);
            chars("char/view/ro/" + k, ByteBuffer.allocate(8).order(o).asCharBuffer().asReadOnlyBuffer(), false);

            // The BYTES, not the round trip. A view that is wrong about its
            // order still reads back what it wrote, so only the underlying
            // buffer can tell the two apart.
            ByteBuffer src = ByteBuffer.allocate(8).order(o);
            src.asIntBuffer().put(0, 0x01020304);
            p("int view byte pattern " + k, hex(src.array(), 0, 4));
            src = ByteBuffer.allocate(8).order(o);
            src.asShortBuffer().put(0, (short) 0x0102);
            p("short view byte pattern " + k, hex(src.array(), 0, 2));
            src = ByteBuffer.allocate(8).order(o);
            src.asLongBuffer().put(0, 0x0102030405060708L);
            p("long view byte pattern " + k, hex(src.array(), 0, 8));
            src = ByteBuffer.allocate(8).order(o);
            src.asCharBuffer().put(0, '\u0041');
            p("char view byte pattern " + k, hex(src.array(), 0, 2));
            src = ByteBuffer.allocate(8).order(o);
            src.asFloatBuffer().put(0, 1.0f);
            p("float view byte pattern " + k, hex(src.array(), 0, 4));
            src = ByteBuffer.allocate(8).order(o);
            src.asDoubleBuffer().put(0, 1.0d);
            p("double view byte pattern " + k, hex(src.array(), 0, 8));

            // `slice()` on a VIEW keeps the view's order — unlike `ByteBuffer
            // .slice()`, which resets to BIG_ENDIAN (the quirk W4Nio pins).
            p("int view slice order " + k, ByteBuffer.allocate(16).order(o).asIntBuffer().slice().order());
            p("int view duplicate order " + k, ByteBuffer.allocate(16).order(o).asIntBuffer().duplicate().order());
            p("bytebuffer slice order " + k, ByteBuffer.allocate(16).order(o).slice().order());
            // The view's class NAME encodes backing and order; it is the
            // identity claim this arm exists to compare.
            p("int view class " + k, ByteBuffer.allocate(16).order(o).asIntBuffer().getClass().getName());
            p("char view class " + k, ByteBuffer.allocate(8).order(o).asCharBuffer().getClass().getName());
            p("char ro view class " + k,
              ByteBuffer.allocate(8).order(o).asCharBuffer().asReadOnlyBuffer().getClass().getName());
        }

        System.out.println("rows " + rows);
        System.out.println("DONE L4TypedBufferSweep");
    }
}
