// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteOrder;
import jdk.incubator.vector.ByteVector;
import jdk.incubator.vector.DoubleVector;
import jdk.incubator.vector.FloatVector;
import jdk.incubator.vector.IntVector;
import jdk.incubator.vector.LongVector;
import jdk.incubator.vector.ShortVector;
import jdk.incubator.vector.Vector;
import jdk.incubator.vector.VectorMask;
import jdk.incubator.vector.VectorOperators;
import jdk.incubator.vector.VectorSpecies;

/**
 * The Vector API, lane by lane, in RAW BITS, against a real JDK.
 *
 * CratonVM intercepts `jdk.internal.vm.vector.VectorSupport`'s intrinsic
 * entry points (`native-builtins/src/vector_support_intrinsics.rs`) and
 * computes whole vectors in Rust instead of running the JDK's lane-at-a-time
 * Java fallback. That is a shadow over real bytecode, so it has to produce
 * the same answer on every input this probe can reach — and the inputs are
 * chosen to be the ones where a plausible-looking implementation does NOT:
 *
 *   * NaN and both zeros through MIN/MAX — `Math.min` propagates NaN and
 *     orders -0.0 below 0.0; Rust's `f32::min` does neither.
 *   * MIN_VALUE, -1 and overflow through ADD/SUB/MUL/DIV/ABS/NEG on every
 *     integral width, where Java wraps and Rust panics in debug builds.
 *   * shift counts above the lane width, which the JVM masks.
 *   * float->int casts of NaN and of values past the integral range, which
 *     Java saturates.
 *   * unsigned casts (`ZERO_EXTEND`), which differ from signed ones only when
 *     widening.
 *   * MASKED forms of everything, which the Rust side deliberately refuses —
 *     so these rows prove the fallback still runs and still agrees.
 *   * reductions over floats, where the ORDER of accumulation is observable
 *     because addition is not associative.
 *
 * Every value is printed as raw bits (`floatToRawIntBits` /
 * `doubleToRawLongBits`), never as text: `-0.0` and `0.0` print the same, and
 * a NaN payload difference is invisible as a decimal. Diff two runs directly.
 *
 * Run with `--add-modules jdk.incubator.vector --enable-native-access=ALL-UNNAMED`.
 */
public class VectorApiProbe {

    static int rows = 0;

    static void row(String tag, long... bits) {
        StringBuilder sb = new StringBuilder(tag);
        for (long b : bits) {
            sb.append(' ').append(b);
        }
        System.out.println(sb);
        rows++;
    }

    // ---------------------------------------------------------------- helpers

    static long[] bitsOf(IntVector v) {
        int[] a = v.toArray();
        long[] out = new long[a.length];
        for (int i = 0; i < a.length; i++) {
            out[i] = a[i];
        }
        return out;
    }

    static long[] bitsOf(LongVector v) {
        return v.toArray();
    }

    static long[] bitsOf(ShortVector v) {
        short[] a = v.toArray();
        long[] out = new long[a.length];
        for (int i = 0; i < a.length; i++) {
            out[i] = a[i];
        }
        return out;
    }

    static long[] bitsOf(ByteVector v) {
        byte[] a = v.toArray();
        long[] out = new long[a.length];
        for (int i = 0; i < a.length; i++) {
            out[i] = a[i];
        }
        return out;
    }

    static long[] bitsOf(FloatVector v) {
        float[] a = v.toArray();
        long[] out = new long[a.length];
        for (int i = 0; i < a.length; i++) {
            out[i] = Float.floatToRawIntBits(a[i]) & 0xFFFFFFFFL;
        }
        return out;
    }

    static long[] bitsOf(DoubleVector v) {
        double[] a = v.toArray();
        long[] out = new long[a.length];
        for (int i = 0; i < a.length; i++) {
            out[i] = Double.doubleToRawLongBits(a[i]);
        }
        return out;
    }

    public static void main(String[] args) {
        shapeNote();
        ints();
        longs();
        shortsAndBytes();
        floats();
        doubles();
        conversions();
        masked();
        reductions();
        memory();
        System.out.println("CK VectorApiProbe rows=" + rows);
        System.out.println("PASS VectorApiProbe");
    }

    /**
     * The one thing that legitimately differs between VMs, printed as a NOTE so
     * a reader sees it and a diff of the rows does not trip on it.
     *
     * `SPECIES_MAX` / `SPECIES_PREFERRED` are derived from
     * `VectorSupport.getMaxLaneCount`, which is a statement about the HOST, not
     * about semantics: a JVM may report any shape it supports. Measured on one
     * machine, Temurin 25.0.3+9 reports 256 bits and CratonVM reports 128 —
     * CratonVM's `getMaxLaneCount` deliberately answers the 128-bit minimum
     * rather than probing the CPU. Every fixed-width species below is
     * enumerated explicitly for exactly this reason; a probe that iterated
     * `SPECIES_MAX` would compare a 256-bit block against a 128-bit one and
     * report 36 spurious row differences.
     */
    static void shapeNote() {
        System.out.println("SHAPE-NOTE maxInt=" + IntVector.SPECIES_MAX.vectorBitSize()
                + " maxFloat=" + FloatVector.SPECIES_MAX.vectorBitSize()
                + " preferredInt=" + IntVector.SPECIES_PREFERRED.vectorBitSize());
    }

    // ------------------------------------------------------------------- ints

    static void ints() {
        for (VectorSpecies<Integer> sp : new VectorSpecies[] {
                IntVector.SPECIES_64, IntVector.SPECIES_128,
                IntVector.SPECIES_256, IntVector.SPECIES_512 }) {
            int n = sp.length();
            int[] a = new int[n];
            int[] b = new int[n];
            for (int i = 0; i < n; i++) {
                // Integer.MIN_VALUE, -1, 0 and a spread — the rows where
                // wrapping, division overflow and abs() all bite.
                a[i] = switch (i % 5) {
                    case 0 -> Integer.MIN_VALUE;
                    case 1 -> -1;
                    case 2 -> 0;
                    case 3 -> Integer.MAX_VALUE;
                    default -> i * 0x01010101;
                };
                b[i] = switch (i % 4) {
                    case 0 -> -1;
                    case 1 -> 3;
                    case 2 -> Integer.MIN_VALUE;
                    default -> 33 + i;
                };
            }
            IntVector va = IntVector.fromArray(sp, a, 0);
            IntVector vb = IntVector.fromArray(sp, b, 0);
            String t = "INT" + sp.vectorBitSize();
            row(t + ".add", bitsOf(va.add(vb)));
            row(t + ".sub", bitsOf(va.sub(vb)));
            row(t + ".mul", bitsOf(va.mul(vb)));
            row(t + ".min", bitsOf(va.min(vb)));
            row(t + ".max", bitsOf(va.max(vb)));
            row(t + ".and", bitsOf(va.and(vb)));
            row(t + ".or", bitsOf(va.or(vb)));
            row(t + ".xor", bitsOf(va.lanewise(VectorOperators.XOR, vb)));
            row(t + ".abs", bitsOf(va.abs()));
            row(t + ".neg", bitsOf(va.neg()));
            // Shifts BY A VECTOR and BY A SCALAR are different VectorSupport
            // entry points (binaryOp vs broadcastInt) and both are intercepted.
            row(t + ".shlv", bitsOf(va.lanewise(VectorOperators.LSHL, vb)));
            row(t + ".shrv", bitsOf(va.lanewise(VectorOperators.ASHR, vb)));
            row(t + ".ushrv", bitsOf(va.lanewise(VectorOperators.LSHR, vb)));
            for (int s : new int[] { 0, 1, 31, 32, 33, -1 }) {
                row(t + ".shls" + s, bitsOf(va.lanewise(VectorOperators.LSHL, s)));
                row(t + ".shrs" + s, bitsOf(va.lanewise(VectorOperators.ASHR, s)));
                row(t + ".ushrs" + s, bitsOf(va.lanewise(VectorOperators.LSHR, s)));
            }
            row(t + ".addscalar", bitsOf(va.add(7)));
            row(t + ".broadcast", bitsOf(IntVector.broadcast(sp, 0xDEADBEEF)));
            row(t + ".zero", bitsOf(IntVector.zero(sp)));
            // Division: a separate row set, because a zero divisor must throw
            // and the vector one above deliberately contains none.
            int[] d = new int[n];
            java.util.Arrays.fill(d, 3);
            d[0] = -1;
            row(t + ".div", bitsOf(va.div(IntVector.fromArray(sp, d, 0))));
            String thrown;
            try {
                int[] z = new int[n];
                va.div(IntVector.fromArray(sp, z, 0));
                thrown = "none";
            } catch (ArithmeticException e) {
                thrown = "ArithmeticException";
            }
            row(t + ".div0." + thrown);
        }
    }

    // ------------------------------------------------------------------ longs

    static void longs() {
        VectorSpecies<Long> sp = LongVector.SPECIES_256;
        int n = sp.length();
        long[] a = new long[n];
        long[] b = new long[n];
        for (int i = 0; i < n; i++) {
            a[i] = switch (i % 4) {
                case 0 -> Long.MIN_VALUE;
                case 1 -> -1L;
                case 2 -> Long.MAX_VALUE;
                default -> 0x0102030405060708L * (i + 1);
            };
            b[i] = switch (i % 3) {
                case 0 -> -1L;
                case 1 -> 65L;
                default -> 7L;
            };
        }
        LongVector va = LongVector.fromArray(sp, a, 0);
        LongVector vb = LongVector.fromArray(sp, b, 0);
        row("LONG.add", bitsOf(va.add(vb)));
        row("LONG.sub", bitsOf(va.sub(vb)));
        row("LONG.mul", bitsOf(va.mul(vb)));
        row("LONG.min", bitsOf(va.min(vb)));
        row("LONG.max", bitsOf(va.max(vb)));
        row("LONG.and", bitsOf(va.and(vb)));
        row("LONG.abs", bitsOf(va.abs()));
        row("LONG.neg", bitsOf(va.neg()));
        row("LONG.shlv", bitsOf(va.lanewise(VectorOperators.LSHL, vb)));
        row("LONG.ushrv", bitsOf(va.lanewise(VectorOperators.LSHR, vb)));
        for (int s : new int[] { 0, 63, 64, 65 }) {
            row("LONG.shls" + s, bitsOf(va.lanewise(VectorOperators.LSHL, s)));
            row("LONG.ushrs" + s, bitsOf(va.lanewise(VectorOperators.LSHR, s)));
        }
        row("LONG.broadcast", bitsOf(LongVector.broadcast(sp, Long.MIN_VALUE)));
    }

    // -------------------------------------------------------- shorts and bytes

    static void shortsAndBytes() {
        VectorSpecies<Short> ssp = ShortVector.SPECIES_128;
        int sn = ssp.length();
        short[] sa = new short[sn];
        short[] sb = new short[sn];
        for (int i = 0; i < sn; i++) {
            sa[i] = (short) (i % 3 == 0 ? Short.MIN_VALUE : (i * 4097));
            sb[i] = (short) (i % 2 == 0 ? -1 : 17);
        }
        ShortVector sva = ShortVector.fromArray(ssp, sa, 0);
        ShortVector svb = ShortVector.fromArray(ssp, sb, 0);
        row("SHORT.add", bitsOf(sva.add(svb)));
        row("SHORT.mul", bitsOf(sva.mul(svb)));
        row("SHORT.abs", bitsOf(sva.abs()));
        row("SHORT.neg", bitsOf(sva.neg()));
        // Shift counts ACROSS the lane width. A short lane masks the count by
        // 15 and a byte lane by 7 — not by 31, which is what an implementation
        // written for `int` would do, and what the counts below separate. The
        // `>>>` rows also separate zero-extension AT THE LANE WIDTH from
        // zero-extension at 32 bits, which agree for every non-negative lane
        // and disagree for `Short.MIN_VALUE`.
        for (int s : new int[] { 0, 1, 15, 16, 17, 31, 32, -1 }) {
            row("SHORT.shls" + s, bitsOf(sva.lanewise(VectorOperators.LSHL, s)));
            row("SHORT.shrs" + s, bitsOf(sva.lanewise(VectorOperators.ASHR, s)));
            row("SHORT.ushrs" + s, bitsOf(sva.lanewise(VectorOperators.LSHR, s)));
        }
        row("SHORT.shlv", bitsOf(sva.lanewise(VectorOperators.LSHL, svb)));
        row("SHORT.ushrv", bitsOf(sva.lanewise(VectorOperators.LSHR, svb)));

        VectorSpecies<Byte> bsp = ByteVector.SPECIES_128;
        int bn = bsp.length();
        byte[] ba = new byte[bn];
        byte[] bb = new byte[bn];
        for (int i = 0; i < bn; i++) {
            ba[i] = (byte) (i % 3 == 0 ? Byte.MIN_VALUE : (i * 37));
            bb[i] = (byte) (i % 2 == 0 ? -1 : 5);
        }
        ByteVector bva = ByteVector.fromArray(bsp, ba, 0);
        ByteVector bvb = ByteVector.fromArray(bsp, bb, 0);
        row("BYTE.add", bitsOf(bva.add(bvb)));
        row("BYTE.mul", bitsOf(bva.mul(bvb)));
        row("BYTE.abs", bitsOf(bva.abs()));
        row("BYTE.neg", bitsOf(bva.neg()));
        for (int s : new int[] { 0, 1, 7, 8, 9, 31, 32, -1 }) {
            row("BYTE.shls" + s, bitsOf(bva.lanewise(VectorOperators.LSHL, s)));
            row("BYTE.shrs" + s, bitsOf(bva.lanewise(VectorOperators.ASHR, s)));
            row("BYTE.ushrs" + s, bitsOf(bva.lanewise(VectorOperators.LSHR, s)));
        }
        row("BYTE.shlv", bitsOf(bva.lanewise(VectorOperators.LSHL, bvb)));
        row("BYTE.ushrv", bitsOf(bva.lanewise(VectorOperators.LSHR, bvb)));
    }

    // ----------------------------------------------------------------- floats

    static final float[] FLOAT_SPREAD = {
        Float.NaN, 0.0f, -0.0f, 1.0f, -1.0f,
        Float.MIN_VALUE, Float.MAX_VALUE,
        Float.POSITIVE_INFINITY, Float.NEGATIVE_INFINITY,
        0.1f, -3.5f, 1e30f, -1e-30f, 2.5f, 16777217.0f, -16777217.0f,
    };

    static void floats() {
        for (VectorSpecies<Float> sp : new VectorSpecies[] {
                FloatVector.SPECIES_128, FloatVector.SPECIES_256,
                FloatVector.SPECIES_512 }) {
            int n = sp.length();
            float[] a = new float[n];
            float[] b = new float[n];
            float[] c = new float[n];
            for (int i = 0; i < n; i++) {
                a[i] = FLOAT_SPREAD[i % FLOAT_SPREAD.length];
                b[i] = FLOAT_SPREAD[(i + 3) % FLOAT_SPREAD.length];
                c[i] = FLOAT_SPREAD[(i + 7) % FLOAT_SPREAD.length];
            }
            FloatVector va = FloatVector.fromArray(sp, a, 0);
            FloatVector vb = FloatVector.fromArray(sp, b, 0);
            FloatVector vc = FloatVector.fromArray(sp, c, 0);
            String t = "FLOAT" + sp.vectorBitSize();
            row(t + ".add", bitsOf(va.add(vb)));
            row(t + ".sub", bitsOf(va.sub(vb)));
            row(t + ".mul", bitsOf(va.mul(vb)));
            row(t + ".div", bitsOf(va.div(vb)));
            row(t + ".min", bitsOf(va.min(vb)));
            row(t + ".max", bitsOf(va.max(vb)));
            row(t + ".abs", bitsOf(va.abs()));
            row(t + ".neg", bitsOf(va.neg()));
            row(t + ".sqrt", bitsOf(va.lanewise(VectorOperators.SQRT)));
            // fma: the whole point of the ternary entry point, and the one
            // where a naive `a*b+c` is a DIFFERENT answer.
            row(t + ".fma", bitsOf(va.fma(vb, vc)));
            row(t + ".broadcast", bitsOf(FloatVector.broadcast(sp, -0.0f)));
            row(t + ".broadcastnan", bitsOf(FloatVector.broadcast(sp, Float.NaN)));
            row(t + ".zero", bitsOf(FloatVector.zero(sp)));
            row(t + ".addscalar", bitsOf(va.add(0.5f)));
        }
    }

    static void doubles() {
        VectorSpecies<Double> sp = DoubleVector.SPECIES_256;
        int n = sp.length();
        double[] a = new double[n];
        double[] b = new double[n];
        double[] c = new double[n];
        double[] spread = {
            Double.NaN, 0.0, -0.0, 1.0, -1.0, Double.MIN_VALUE, Double.MAX_VALUE,
            Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, 0.1, -3.5,
        };
        for (int i = 0; i < n; i++) {
            a[i] = spread[i % spread.length];
            b[i] = spread[(i + 2) % spread.length];
            c[i] = spread[(i + 5) % spread.length];
        }
        DoubleVector va = DoubleVector.fromArray(sp, a, 0);
        DoubleVector vb = DoubleVector.fromArray(sp, b, 0);
        DoubleVector vc = DoubleVector.fromArray(sp, c, 0);
        row("DOUBLE.add", bitsOf(va.add(vb)));
        row("DOUBLE.sub", bitsOf(va.sub(vb)));
        row("DOUBLE.mul", bitsOf(va.mul(vb)));
        row("DOUBLE.div", bitsOf(va.div(vb)));
        row("DOUBLE.min", bitsOf(va.min(vb)));
        row("DOUBLE.max", bitsOf(va.max(vb)));
        row("DOUBLE.abs", bitsOf(va.abs()));
        row("DOUBLE.neg", bitsOf(va.neg()));
        row("DOUBLE.sqrt", bitsOf(va.lanewise(VectorOperators.SQRT)));
        row("DOUBLE.fma", bitsOf(va.fma(vb, vc)));
        row("DOUBLE.broadcast", bitsOf(DoubleVector.broadcast(sp, -0.0)));
    }

    // ----------------------------------------------------------- conversions

    static void conversions() {
        VectorSpecies<Integer> isp = IntVector.SPECIES_256;
        VectorSpecies<Float> fsp = FloatVector.SPECIES_256;
        int n = isp.length();
        int[] ia = new int[n];
        for (int i = 0; i < n; i++) {
            ia[i] = switch (i % 5) {
                case 0 -> Integer.MIN_VALUE;
                case 1 -> -1;
                case 2 -> 0x7F800000;   // +Inf as a float bit pattern
                case 3 -> 0x7FC00000;   // NaN as a float bit pattern
                default -> i * 1000003;
            };
        }
        IntVector iv = IntVector.fromArray(isp, ia, 0);
        // REINTERPRET: same bits, read as floats. This is the operation
        // GPULlama3's kernel uses to rebuild binary32 by hand.
        row("CONV.i2f.reinterpret", bitsOf(iv.reinterpretAsFloats()));
        row("CONV.f2i.reinterpret", bitsOf(iv.reinterpretAsFloats().reinterpretAsInts()));
        // CAST: values, not bits.
        row("CONV.i2f.cast", bitsOf((FloatVector) iv.castShape(fsp, 0)));
        float[] fa = new float[n];
        for (int i = 0; i < n; i++) {
            fa[i] = FLOAT_SPREAD[i % FLOAT_SPREAD.length];
        }
        FloatVector fv = FloatVector.fromArray(fsp, fa, 0);
        row("CONV.f2i.cast", bitsOf((IntVector) fv.castShape(isp, 0)));
        // A widening cast that crosses shapes: short -> int, which is the
        // `castShape` GPULlama3 uses on the FP16 halves.
        VectorSpecies<Short> ssp = ShortVector.SPECIES_128;
        short[] sa = new short[ssp.length()];
        for (int i = 0; i < sa.length; i++) {
            sa[i] = (short) (i % 2 == 0 ? -1 : (0x3C00 + i));
        }
        ShortVector sv = ShortVector.fromArray(ssp, sa, 0);
        row("CONV.s2i.cast", bitsOf((IntVector) sv.castShape(isp, 0)));
        row("CONV.s2i.zeroext",
                bitsOf((IntVector) sv.convertShape(VectorOperators.ZERO_EXTEND_S2I, isp, 0)));
        row("CONV.i2s.cast", bitsOf((ShortVector) iv.castShape(ssp, 0)));
        // Reinterpret across lane widths — the RESHAPE case the Rust side
        // deliberately refuses, so these rows prove the fallback is right.
        row("CONV.i2l.reinterpret",
                bitsOf(iv.reinterpretShape(LongVector.SPECIES_256, 0).reinterpretAsLongs()));
        row("CONV.i2b.reinterpret",
                bitsOf(iv.reinterpretShape(ByteVector.SPECIES_256, 0).reinterpretAsBytes()));
    }

    // --------------------------------------------------------------- masked

    /**
     * Masked forms. The Rust side refuses every one of these and hands the call
     * back to the JDK's lambda, so these rows are the evidence that the refusal
     * path is wired up and still correct — a fallback that silently produced an
     * unmasked answer would show here and nowhere else.
     */
    static void masked() {
        VectorSpecies<Integer> sp = IntVector.SPECIES_256;
        int n = sp.length();
        int[] a = new int[n];
        int[] b = new int[n];
        boolean[] m = new boolean[n];
        for (int i = 0; i < n; i++) {
            a[i] = i * 7 - 3;
            b[i] = (i % 3) + 1;
            m[i] = (i % 2) == 0;
        }
        IntVector va = IntVector.fromArray(sp, a, 0);
        IntVector vb = IntVector.fromArray(sp, b, 0);
        VectorMask<Integer> mask = VectorMask.fromArray(sp, m, 0);
        row("MASK.add", bitsOf(va.add(vb, mask)));
        row("MASK.sub", bitsOf(va.sub(vb, mask)));
        row("MASK.mul", bitsOf(va.mul(vb, mask)));
        row("MASK.abs", bitsOf(va.lanewise(VectorOperators.ABS, mask)));
        row("MASK.neg", bitsOf(va.lanewise(VectorOperators.NEG, mask)));
        row("MASK.shl", bitsOf(va.lanewise(VectorOperators.LSHL, 3, mask)));
        row("MASK.blend", bitsOf(va.blend(vb, mask)));
        row("MASK.reduceadd", va.reduceLanes(VectorOperators.ADD, mask));
        row("MASK.truecount", mask.trueCount());
        VectorSpecies<Float> fsp = FloatVector.SPECIES_256;
        float[] fa = new float[fsp.length()];
        float[] fb = new float[fsp.length()];
        float[] fc = new float[fsp.length()];
        boolean[] fm = new boolean[fsp.length()];
        for (int i = 0; i < fa.length; i++) {
            fa[i] = FLOAT_SPREAD[i % FLOAT_SPREAD.length];
            fb[i] = FLOAT_SPREAD[(i + 1) % FLOAT_SPREAD.length];
            fc[i] = FLOAT_SPREAD[(i + 2) % FLOAT_SPREAD.length];
            fm[i] = (i % 3) != 0;
        }
        VectorMask<Float> fmask = VectorMask.fromArray(fsp, fm, 0);
        row("MASK.fma", bitsOf(FloatVector.fromArray(fsp, fa, 0)
                .fma(FloatVector.fromArray(fsp, fb, 0), FloatVector.fromArray(fsp, fc, 0))
                .blend(FloatVector.zero(fsp), fmask)));
        row("MASK.fdiv", bitsOf(FloatVector.fromArray(fsp, fa, 0)
                .div(FloatVector.fromArray(fsp, fb, 0), fmask)));
    }

    // ------------------------------------------------------------ reductions

    static void reductions() {
        VectorSpecies<Integer> isp = IntVector.SPECIES_256;
        int n = isp.length();
        int[] a = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = switch (i % 4) {
                case 0 -> Integer.MIN_VALUE;
                case 1 -> -1;
                case 2 -> Integer.MAX_VALUE;
                default -> i * 131071;
            };
        }
        IntVector iv = IntVector.fromArray(isp, a, 0);
        row("RED.int.add", iv.reduceLanes(VectorOperators.ADD));
        row("RED.int.mul", iv.reduceLanes(VectorOperators.MUL));
        row("RED.int.min", iv.reduceLanes(VectorOperators.MIN));
        row("RED.int.max", iv.reduceLanes(VectorOperators.MAX));
        row("RED.int.and", iv.reduceLanes(VectorOperators.AND));
        row("RED.int.or", iv.reduceLanes(VectorOperators.OR));
        row("RED.int.xor", iv.reduceLanes(VectorOperators.XOR));

        VectorSpecies<Float> fsp = FloatVector.SPECIES_256;
        float[] fa = new float[fsp.length()];
        for (int i = 0; i < fa.length; i++) {
            // Magnitudes chosen so a different accumulation ORDER gives a
            // different sum — float addition is not associative and the
            // reduction order is observable.
            fa[i] = (i % 2 == 0) ? 1.0e8f : 1.0f;
        }
        FloatVector fv = FloatVector.fromArray(fsp, fa, 0);
        row("RED.float.add", Float.floatToRawIntBits(fv.reduceLanes(VectorOperators.ADD)));
        row("RED.float.mul", Float.floatToRawIntBits(fv.reduceLanes(VectorOperators.MUL)));
        row("RED.float.min", Float.floatToRawIntBits(fv.reduceLanes(VectorOperators.MIN)));
        row("RED.float.max", Float.floatToRawIntBits(fv.reduceLanes(VectorOperators.MAX)));
        float[] withNan = fa.clone();
        withNan[1] = Float.NaN;
        withNan[2] = -0.0f;
        FloatVector nv = FloatVector.fromArray(fsp, withNan, 0);
        row("RED.float.addnan", Float.floatToRawIntBits(nv.reduceLanes(VectorOperators.ADD)));
        row("RED.float.minnan", Float.floatToRawIntBits(nv.reduceLanes(VectorOperators.MIN)));
        row("RED.float.maxnan", Float.floatToRawIntBits(nv.reduceLanes(VectorOperators.MAX)));

        VectorSpecies<Long> lsp = LongVector.SPECIES_256;
        long[] la = new long[lsp.length()];
        for (int i = 0; i < la.length; i++) {
            la[i] = i % 2 == 0 ? Long.MIN_VALUE : 0x0F0F0F0F0F0F0F0FL;
        }
        LongVector lv = LongVector.fromArray(lsp, la, 0);
        row("RED.long.add", lv.reduceLanes(VectorOperators.ADD));
        row("RED.long.min", lv.reduceLanes(VectorOperators.MIN));
        row("RED.long.and", lv.reduceLanes(VectorOperators.AND));
    }

    // ---------------------------------------------------------------- memory

    /** Loads and stores, both array-backed and `MemorySegment`-backed. */
    static void memory() {
        VectorSpecies<Float> fsp = FloatVector.SPECIES_256;
        int n = fsp.length();
        float[] src = new float[n * 2];
        for (int i = 0; i < src.length; i++) {
            src[i] = FLOAT_SPREAD[i % FLOAT_SPREAD.length];
        }
        row("MEM.fromArray0", bitsOf(FloatVector.fromArray(fsp, src, 0)));
        row("MEM.fromArrayN", bitsOf(FloatVector.fromArray(fsp, src, n)));
        float[] dst = new float[n * 2];
        FloatVector.fromArray(fsp, src, 0).intoArray(dst, n);
        long[] db = new long[dst.length];
        for (int i = 0; i < dst.length; i++) {
            db[i] = Float.floatToRawIntBits(dst[i]) & 0xFFFFFFFFL;
        }
        row("MEM.intoArray", db);

        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(n * 8L, 8);
            for (int i = 0; i < n * 2; i++) {
                seg.set(ValueLayout.JAVA_FLOAT, i * 4L, src[i]);
            }
            row("MEM.fromSegment",
                    bitsOf(FloatVector.fromMemorySegment(fsp, seg, 0, ByteOrder.LITTLE_ENDIAN)));
            MemorySegment heap = MemorySegment.ofArray(new byte[n * 4]);
            FloatVector.fromArray(fsp, src, 0)
                    .intoMemorySegment(heap, 0, ByteOrder.LITTLE_ENDIAN);
            long[] hb = new long[n];
            for (int i = 0; i < n; i++) {
                hb[i] = heap.get(ValueLayout.JAVA_INT_UNALIGNED, i * 4L) & 0xFFFFFFFFL;
            }
            row("MEM.intoSegment", hb);
            // Big-endian, which swaps the bytes on the way in and out.
            row("MEM.fromSegmentBE",
                    bitsOf(FloatVector.fromMemorySegment(fsp, seg, 0, ByteOrder.BIG_ENDIAN)));
        }

        VectorSpecies<Short> ssp = ShortVector.SPECIES_128;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(ssp.length() * 2L, 8);
            for (int i = 0; i < ssp.length(); i++) {
                seg.set(ValueLayout.JAVA_SHORT, i * 2L, (short) (i % 2 == 0 ? -1 : 0x3C00 + i));
            }
            row("MEM.shortFromSegment",
                    bitsOf(ShortVector.fromMemorySegment(ssp, seg, 0, ByteOrder.LITTLE_ENDIAN)));
        }
    }
}
