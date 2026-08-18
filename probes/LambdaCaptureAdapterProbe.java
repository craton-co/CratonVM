import java.util.function.*;

/**
 * The shapes a hand-emitted inline-cache thunk has to get right once the lambda
 * CAPTURES.
 *
 * {@code LambdaAdapterProbe} covers the non-capturing thunk, which is a pure
 * register shuffle. Adding captures adds four things a shuffle alone could not
 * get wrong:
 *
 *   - the OFFSET each capture is read from (uniform 16-byte cells from the
 *     object header, one per capture, in impl-parameter order — read capture 1
 *     at capture 0's offset and both lambdas below return the same wrong
 *     answer);
 *   - the WIDTH and SIGN of the load (a `byte` capture of -100 read as an
 *     unsigned byte is 156; a `float` read sign-extended is a different number
 *     entirely; a `long` read as 32 bits truncates);
 *   - the ORDER of the shuffle against the loads (the SAM arguments now move UP
 *     past the captures, and the receiver has to be saved before the first
 *     capture overwrites it);
 *   - and INSTANCE independence — the thunk is built once per proxy CLASS, so
 *     two lambdas of the same shape with different captured values must not
 *     answer with each other's. Arm 16 is the one that would catch a thunk that
 *     baked its captures instead of loading them.
 *
 * <h2>Why every loop is a separate one-line method</h2>
 *
 * Same reason as {@code LambdaAdapterProbe}, and the same trap: a thunk exists
 * only once the CALLING frame is compiled. With every loop inlined into
 * {@code main} the census reads {@code site_adapters=0} and the file agrees
 * with HotSpot about the interpreter. Read {@code CRATONVM_DBG=lambda-jit}'s
 * {@code site_adapters} before believing any run of this.
 *
 * <h2>Why every captured value goes through a one-line identity method</h2>
 *
 * {@code final int k = 7;} is a CONSTANT VARIABLE in the JLS sense, and javac
 * replaces every reference to one with its value before the lambda is desugared
 * — so {@code v -> v + k} compiles to a body that captures NOTHING, and a probe
 * written the obvious way tests the non-capturing thunk seventeen times over
 * while its comments claim otherwise. Routing each value through {@code i32} and
 * friends below makes the initializer a non-constant expression, which is what
 * forces a real capture. Simply dropping {@code final} would do as well — only a
 * final variable can be a constant variable — but a missing keyword is not
 * something a later edit can be expected to preserve on purpose, and these hops
 * are. {@code javap -p -c} on this class is the check: every
 * {@code lambda$main$N} must take more parameters than its SAM does.
 */
public class LambdaCaptureAdapterProbe {

    private static final int N = 300_000;

    interface IntOp { int apply(int n); }
    interface LongOp { long apply(int n); }
    interface DoubleOp { double apply(int n); }
    interface ObjOp { String apply(String s); }
    interface NoArg { int get(); }
    interface Obj2 { String apply(String a, String b); }

    // ---- identity hops, so each captured value is a real capture ----
    // See the class note: a final local with a constant initializer is inlined
    // by javac and never captured at all.

    private static int i32(int v) { return v; }
    private static long i64(long v) { return v; }
    private static double f64(double v) { return v; }
    private static float f32(float v) { return v; }
    private static byte i8(byte v) { return v; }
    private static char u16(char v) { return v; }
    private static short i16(short v) { return v; }
    private static boolean bool(boolean v) { return v; }
    private static String str(String v) { return v; }

    // ---- the loops: one shape each, nothing else in the frame ----

    private static long loopInt(IntOp op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply(i & 0xFF);
        return s;
    }

    private static long loopLong(LongOp op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply(i & 0xFF);
        return s;
    }

    private static double loopDouble(DoubleOp op) {
        double s = 0;
        for (int i = 0; i < N; i++) s += op.apply(i & 0xFF);
        return s;
    }

    private static long loopObj(ObjOp op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply("q" + (i & 3)).length();
        return s;
    }

    private static long loopObj2(Obj2 op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply("a" + (i & 3), "b" + (i & 1)).length();
        return s;
    }

    private static long loopNoArg(NoArg op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.get();
        return s;
    }

    private static long loopTwo(IntOp a, IntOp b) {
        long s = 0;
        for (int i = 0; i < N; i++) s += ((i & 1) == 0 ? a : b).apply(i & 0xFF);
        return s;
    }

    private static long loopThrowing(IntOp op, int[] caught) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            int arg = (i > 1000 && i % 7000 == 0) ? 4242 : (i & 0xFF);
            try {
                s += op.apply(arg);
            } catch (IllegalStateException e) {
                caught[0]++;
            }
        }
        return s;
    }

    public static void main(String[] args) {
        // 1 — ONE int capture. The minimal capturing thunk: save the receiver,
        // load one cell, and the SAM argument does not move at all (one capture
        // replaces one receiver).
        final int cap1 = i32(1_000_003);
        System.out.println("1 one_int_capture=" + loopInt(n -> cap1 + n));

        // 2 — TWO captures, NON-commutative. The SAM argument now slides UP one
        // register, and swapping the two captures changes the answer.
        final int a2 = i32(900_007), b2 = i32(13);
        System.out.println("2 two_captures=" + loopInt(n -> a2 - b2 * n));

        // 3 — THREE captures, each weighted differently, so any permutation of
        // the cell offsets shows up in the sum.
        final int p = i32(2), q = i32(30), r = i32(700);
        System.out.println("3 three_captures=" + loopInt(n -> p + 10 * q + 100 * r + n));

        // 4 — a `long` capture: full 64-bit width out of the cell's wide
        // payload. A 32-bit load truncates it and a sign-extending one is
        // wrong in the other direction.
        final long big = i64(4_000_000_029L);
        System.out.println("4 long_capture=" + loopLong(n -> big + n));

        // 5 — a `double` capture. The JIT ABI carries it as raw bits in an
        // integer register, so the thunk's wide load is exactly right — and
        // this is the arm that says so rather than assuming it.
        final double d = f64(1.0 / 3.0);
        System.out.println("5 double_capture=" + loopDouble(n -> d * n));

        // 6 — a `float` capture. Its bits are the cell's NARROW payload,
        // zero-extended; sign-extending them yields a different float.
        final float f = f32(0.15625f);
        System.out.println("6 float_capture=" + loopDouble(n -> f * n));

        // 7 — a NEGATIVE `byte` capture. Read as unsigned this is 156, not
        // -100, and every sum below differs.
        final byte nb = i8((byte) -100);
        System.out.println("7 byte_capture=" + loopInt(n -> nb + n));

        // 8 — a `char` capture above 0x7FFF, which must NOT come back negative.
        final char hc = u16((char) 0xFFFF);
        System.out.println("8 char_capture=" + loopInt(n -> hc + n));

        // 9 — a `short` capture, negative, and a `boolean` alongside it: the
        // int category shares one cell payload and one load.
        final short ns = i16((short) -30_000);
        final boolean flag = bool(true);
        System.out.println("9 short_bool_capture=" + loopInt(n -> (flag ? ns : 0) + n));

        // 10 — a REFERENCE capture. The wide payload again, but a pointer:
        // a wrong offset here is a crash, not a wrong number.
        final String prefix = str("captured-");
        System.out.println("10 ref_capture=" + loopObj(s -> prefix + s));

        // 11 — a NULL reference capture. The cell's payload is zero and the
        // impl must receive `null`, not a wild pointer.
        final String nothing = str(null);
        System.out.println("11 null_capture=" + loopObj(s -> (nothing == null ? "N" : "?") + s));

        // 12 — MIXED capture widths in one lambda, in an order where reading
        // any of them at the wrong cell gives a different answer.
        final int m1 = i32(7);
        final long m2 = i64(5_000_000_000L);
        final String m3 = str("xyz");
        System.out.println("12 mixed_captures="
                + loopLong(n -> m1 + m2 + m3.length() + n));

        // 13 — a capture with ZERO SAM arguments: the receiver is replaced by
        // the capture and nothing slides.
        final int only = i32(424_242);
        System.out.println("13 capture_no_args=" + loopNoArg(() -> only));

        // 14 — captures AND reference arguments together, non-commutative in
        // the arguments so an argument slide in the wrong direction shows.
        final String sep = str("|");
        System.out.println("14 capture_two_ref_args=" + loopObj2((x, y) -> x + sep + y + y));

        // 15 — a capturing and a NON-capturing lambda at one call site, so it
        // goes polymorphic across two different thunk shapes.
        final int adj = i32(5);
        System.out.println("15 polymorphic=" + loopTwo(n -> n + adj, n -> n * 5));

        // 16 — TWO INSTANCES OF THE SAME LAMBDA, different captured values.
        // One proxy class, one thunk, two answers. A thunk that baked its
        // capture instead of loading it passes every arm above and fails here.
        System.out.println("16 per_instance=" + loopTwo(instanceOf(3), instanceOf(70_000)));

        // 17 — an exception out of a warm, thunk-dispatched CAPTURING body must
        // still reach the handler at the call site.
        final int[] caught = new int[1];
        final int bump = i32(11);
        long s17 = loopThrowing(n -> {
            if (n == 4242) throw new IllegalStateException("boom");
            return n + bump;
        }, caught);
        System.out.println("17 throwing=" + s17 + " caught=" + caught[0]);

        System.out.println("ALL-DONE");
    }

    /** Two proxies of the same class, differing only in what they captured. */
    private static IntOp instanceOf(int captured) {
        return n -> captured * 2 + n;
    }
}
