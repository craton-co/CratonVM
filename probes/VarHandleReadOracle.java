import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.invoke.WrongMethodTypeException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;

/**
 * Correctness pin for the `VarHandle` read-mode thin direct-call helpers
 * (`vm/src/jit/helpers.rs::jit_varhandle_read_direct`, bound by
 * `cratonvm_jit::VARHANDLE_READ_DIRECT_FNS`).
 *
 * The helpers answer a signature-polymorphic native out of the VM, bypassing
 * the dispatch funnel entirely, so the question is not "is it fast" but "does
 * it still agree". This probe must print **byte-for-byte the same output on
 * HotSpot and on CratonVM**, and the same output with the bind switched off as
 * with it on. Three arms, one file:
 *
 *   java                                            -cp out VarHandleReadOracle
 *   cratonvm --java-home <jdk>                      -cp out VarHandleReadOracle
 *   CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS=0 cratonvm ... VarHandleReadOracle
 *
 * What each section pins, and why it is here rather than a round number:
 *
 *  * **Every primitive kind, every read mode.** The bind is 32 slots — four
 *    access modes crossed with eight primitive returns — and the slot is what
 *    tells the helper which return kind the read is checked against. A slot
 *    mis-wiring reads the RIGHT field with the WRONG width, which is invisible
 *    on an `int` field holding a small number and loud on `long`, `double`,
 *    `char` and `boolean`. All eight are read here at values a truncation or a
 *    sign-extension bug cannot survive: `Long.MIN_VALUE`, a `double` NaN
 *    payload, `char` 0xFFFF, `byte` -1, `boolean` true.
 *
 *  * **The shapes the bind must REFUSE.** A static-field handle (no
 *    coordinates), an array-element handle and a `ByteBuffer` view handle
 *    (two coordinates) must keep the ordinary dispatch and keep answering. If
 *    the descriptor filter ever loosened, these are the rows that would start
 *    reading a coordinate list of the wrong shape.
 *
 *  * **The wrong-type rule.** `(String) intVarHandle.get(obj)` must raise
 *    `WrongMethodTypeException` (JDK's access-mode type check). Reference
 *    returns are deliberately NOT bound for exactly this reason — the rule
 *    reads the call site's declared class, which a baked direct call cannot
 *    carry — so this row is the pin that says the refusal is real and not
 *    just claimed.
 *
 *  * **Null.** A null coordinate must be an NPE from the access, not a
 *    fabricated zero and not a `i64::MIN` sentinel leaking out as a value.
 *
 *  * **A loop.** Every value is read back inside a hot loop as well as once,
 *    so the answers come out of JIT-compiled code — including the OSR body,
 *    which is the door this bind exists for — and not only out of the
 *    interpreter.
 */
public final class VarHandleReadOracle {

    /** The shape netty reads on every buffer accessor: an `int` instance field. */
    static final class Holder {
        boolean z;
        byte b;
        char c;
        short s;
        int i;
        long j;
        float f;
        double d;
        Object ref;
        int[] arr;
    }

    static final class Statics {
        static int si = 0x0BADCAFE;
        static long sj = -1L;
    }

    private static final MethodHandles.Lookup L = MethodHandles.lookup();

    private static VarHandle vh(String name, Class<?> type) throws Exception {
        return L.findVarHandle(Holder.class, name, type);
    }

    private static long sink;

    public static void main(String[] args) throws Exception {
        Holder h = new Holder();
        h.z = true;
        h.b = (byte) -1;
        h.c = (char) 0xFFFF;
        h.s = (short) -32768;
        h.i = 0x7F1E2D3C;
        h.j = Long.MIN_VALUE;
        h.f = Float.intBitsToFloat(0x7FC00001);
        h.d = Double.longBitsToDouble(0x7FF8000000000001L);
        h.ref = "referent";
        h.arr = new int[] { 11, 22, 33 };

        VarHandle vz = vh("z", boolean.class);
        VarHandle vb = vh("b", byte.class);
        VarHandle vc = vh("c", char.class);
        VarHandle vs = vh("s", short.class);
        VarHandle vi = vh("i", int.class);
        VarHandle vj = vh("j", long.class);
        VarHandle vf = vh("f", float.class);
        VarHandle vd = vh("d", double.class);
        VarHandle vref = vh("ref", Object.class);
        VarHandle varr = vh("arr", int[].class);

        // Warm first, so every print below is produced by compiled code. The
        // loop body is its own method called many times AND a long-running
        // loop, because the two compile through different doors (callee
        // compile vs OSR) and the bind has to land at both.
        for (int r = 0; r < 200_000; r++) {
            sink += hotRoundTrip(vz, vb, vc, vs, vi, vj, vf, vd, h);
        }

        System.out.println("== plain get, every primitive kind ==");
        System.out.println("z    = " + (boolean) vz.get(h));
        System.out.println("b    = " + (byte) vb.get(h));
        System.out.println("c    = " + (int) (char) vc.get(h));
        System.out.println("s    = " + (short) vs.get(h));
        System.out.println("i    = " + (int) vi.get(h));
        System.out.println("j    = " + (long) vj.get(h));
        System.out.println("f    = " + Float.floatToRawIntBits((float) vf.get(h)));
        System.out.println("d    = " + Double.doubleToRawLongBits((double) vd.get(h)));

        System.out.println("== the other three read modes agree with get ==");
        System.out.println("vol  = " + (boolean) vz.getVolatile(h) + " " + (byte) vb.getVolatile(h)
                + " " + (int) (char) vc.getVolatile(h) + " " + (short) vs.getVolatile(h)
                + " " + (int) vi.getVolatile(h) + " " + (long) vj.getVolatile(h)
                + " " + Float.floatToRawIntBits((float) vf.getVolatile(h))
                + " " + Double.doubleToRawLongBits((double) vd.getVolatile(h)));
        System.out.println("opq  = " + (boolean) vz.getOpaque(h) + " " + (byte) vb.getOpaque(h)
                + " " + (int) (char) vc.getOpaque(h) + " " + (short) vs.getOpaque(h)
                + " " + (int) vi.getOpaque(h) + " " + (long) vj.getOpaque(h)
                + " " + Float.floatToRawIntBits((float) vf.getOpaque(h))
                + " " + Double.doubleToRawLongBits((double) vd.getOpaque(h)));
        System.out.println("acq  = " + (boolean) vz.getAcquire(h) + " " + (byte) vb.getAcquire(h)
                + " " + (int) (char) vc.getAcquire(h) + " " + (short) vs.getAcquire(h)
                + " " + (int) vi.getAcquire(h) + " " + (long) vj.getAcquire(h)
                + " " + Float.floatToRawIntBits((float) vf.getAcquire(h))
                + " " + Double.doubleToRawLongBits((double) vd.getAcquire(h)));

        System.out.println("== reads the bind leaves on the funnel ==");
        // Reference returns: never bound, and they still have to answer.
        System.out.println("ref  = " + (Object) vref.get(h));
        System.out.println("arr  = " + Arrays.toString((int[]) varr.get(h)));
        // Zero coordinates: a static-field handle.
        VarHandle vsi = L.findStaticVarHandle(Statics.class, "si", int.class);
        VarHandle vsj = L.findStaticVarHandle(Statics.class, "sj", long.class);
        System.out.println("si   = " + (int) vsi.get());
        System.out.println("sj   = " + (long) vsj.get());
        // Two coordinates: array element and ByteBuffer view.
        VarHandle vae = MethodHandles.arrayElementVarHandle(int[].class);
        System.out.println("ae   = " + (int) vae.get(h.arr, 1));
        ByteBuffer bb = ByteBuffer.allocate(16).order(ByteOrder.BIG_ENDIAN);
        bb.putLong(0, 0x0102030405060708L);
        VarHandle vbv = MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.BIG_ENDIAN);
        System.out.println("bv   = " + (long) vbv.get(bb, 0));

        System.out.println("== the rules a fast path must not paper over ==");
        // A boxed primitive reaching a non-Object reference return.
        try {
            String wrong = (String) vi.get(h);
            System.out.println("wrongType = NO EXCEPTION, got " + wrong);
        } catch (WrongMethodTypeException e) {
            System.out.println("wrongType = WrongMethodTypeException");
        } catch (ClassCastException e) {
            System.out.println("wrongType = ClassCastException");
        }
        // A null coordinate.
        try {
            int npe = (int) vi.get((Holder) null);
            System.out.println("nullRecv  = NO EXCEPTION, got " + npe);
        } catch (NullPointerException e) {
            System.out.println("nullRecv  = NullPointerException");
        }

        System.out.println("== hot-loop checksum ==");
        System.out.println("checksum = " + sink);
    }

    /**
     * One read of every bound kind, folded into a single `long`.
     *
     * Kept small and called from the warm loop so it tiers up as a CALLEE, and
     * containing its own loop so the same reads also run in an OSR body: a
     * bind that landed at only one of those doors would still print the right
     * answers here, but the checksum would come out of the interpreter, so the
     * printed value has to agree across arms either way.
     */
    private static long hotRoundTrip(VarHandle vz, VarHandle vb, VarHandle vc, VarHandle vs,
            VarHandle vi, VarHandle vj, VarHandle vf, VarHandle vd, Holder h) {
        long acc = 0;
        for (int k = 0; k < 4; k++) {
            acc += ((boolean) vz.get(h)) ? 1 : 0;
            acc += (byte) vb.getVolatile(h);
            acc += (char) vc.getOpaque(h);
            acc += (short) vs.getAcquire(h);
            acc += (int) vi.get(h);
            acc += (long) vj.getAcquire(h);
            acc += Float.floatToRawIntBits((float) vf.getVolatile(h));
            acc += Double.doubleToRawLongBits((double) vd.getOpaque(h));
        }
        return acc;
    }
}
