// JIT round 21 wave 3, lane intrinsics: exactness of every JDK method this VM answers with compiled
// code or a registered native instead of its bytecode, against HotSpot 25, in hot compiled loops.
//
// Families (one line per operation; every operation's call site runs >= ITER = 100,000 times per
// pass, two passes, the second one compiled):
//   math.*      Math / StrictMath unary, binary and fma corners (abs of MIN_VALUE, floorDiv /
//               floorMod / ceilDiv with negative divisors and MIN/-1, the *Exact family's
//               "integer overflow" / "long overflow", round(-0.5) / round(NaN) /
//               round(0.49999999999999994), rint ties, ceil / floor of -0.0 and -0.5, signum,
//               copySign with a NaN sign (Math copies it, StrictMath treats it as positive),
//               min / max of +-0.0 and of a NaN with a payload, nextUp / ulp / getExponent,
//               toIntExact, multiplyHigh, clamp)
//   bits.*      Integer / Long numberOfLeadingZeros(0), highestOneBit, reverse(Bytes), rotate by
//               negative / >= width distances, compress / expand, unsigned divide
//   char.*      Character classification on BMP and supplementary code points (isDigit /
//               isLetter of U+1D7CE / U+10400 ...), digit / getNumericValue, case mapping
//   ac.*        System.arraycopy: overlap both ways, every exception with HotSpot's message text
//               ("arraycopy: last source index 11 out of bounds for int[10]", "arraycopy: type
//               mismatch: can not copy object array[] into int[]", element mismatch with the
//               partial copy left behind), NaN payloads preserved
//   ar.*        Arrays.copyOf / copyOfRange / equals / fill / hashCode / mismatch / compare / sort
//   ob.*        identityHashCode / Object.hashCode stability, Thread.currentThread,
//               Class.isInstance / isArray / getModifiers / cast, Objects.requireNonNull /
//               checkIndex messages, box caches, helpful NPE texts from compiled code
//   st.*        String.equals / hashCode / indexOf / compareTo / charAt on both coders
//   sb.*        StringBuilder.append(int / long / char / float / double) digits, insert, reverse
//   nu.* pa.*   Integer / Long toString (MIN_VALUE, every radix), parseInt / parseLong /
//               parseUnsigned* / decode / Byte / Short / Double / Float parse errors and messages
//   dt.*        Double.toString / Float.toString shortest representation (JDK 19+ Raffaello
//               Giulietti algorithm) on a fixed list and three sweeps of raw bit patterns
//   cr.*        CRC32 / CRC32C / Adler32 updates (arrays, ints above 255, heap and direct buffers,
//               range errors)
//   b6.*        Base64 basic / URL / MIME encode-decode and its error messages
//
// A row line is `<label> <fact for each corner input> plain=<fact for the ordinary input> h=<hex>`,
// where `h` folds every one of the ITER results, so a corner that is right in the last call but
// wrong in an earlier (differently compiled) one still shows. `drift <label> pass0=...` follows a
// row whose compiled pass disagreed with its first pass. A fact is a value (doubles / floats as
// raw-bit hex), or `!<exception class>:<message>`. Values an operation COMPUTES from a NaN through
// x87 / libm (%, sqrt of a negative, hypot, atan2, IEEEremainder, fma) are printed canonicalised;
// values it SELECTS (abs, min, max, copySign, floor / ceil of a NaN, signum, nextUp, ulp) raw.
// Signalling NaNs are only fed to StrictMath and to non-rounding Math methods: HotSpot's C2
// answers Math.floor / ceil / rint with ROUNDSD (which quiets them) while its interpreter runs
// the body, so those answers are tier-dependent on HotSpot itself.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21IntrinsicCorners.java):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21IntrinsicCorners                    # default (jdk-only), all tiers
//   cratonvm --compatible -Xmx512m -cp $P R21IntrinsicCorners       # both modes, same lines
//   cratonvm --nojit -Xmx512m -cp $P R21IntrinsicCorners notime     # interpreter control
//   java -Xmx512m -cp $P R21IntrinsicCorners                        # HotSpot 25 (the oracle)
// Optional args: `notime` drops the timing rows; a number replaces ITER (>= 100000 recommended).
//
// Kill-switch controls for this lane's fixes (each changes only the named lines):
//   CRATONVM_JIT_STRICTMATH_ROUNDING_EXACT=0   math.ds.floor / ceil / rint go back to ROUNDSD in
//                                              compiled code: the signalling-NaN corners print a
//                                              QUIET payload (bit 51 set) and rint keeps a NaN's
//                                              sign (h differs, `drift` may appear)
//   CRATONVM_MATH_ROUNDING_NAN_EXACT=0         the natives' NaN answers: math.ds.floor / ceil print
//                                              the host libm's NaN, rint of a signalling NaN stays
//                                              signalling (visible with --nojit)
//   CRATONVM_JIT_ARRAYCOPY_DIRECT_ELEMENT_ATOMIC=0  no line changes here (single-threaded); the
//                                              torn-element race is R20FuzzGTearCopy1's shape
//
// HotSpot expectation: every non-`time` line identical (diff them), no `drift` lines. Timing rows
// `time <row> <N> ns/op` (median of 7 reps after warm-up) are the measurements the proposals in
// docs/internal/proposals/jit/jit-r21-intrinsics-proposals.md are decided by.

import java.nio.ByteBuffer;
import java.util.Arrays;
import java.util.Base64;
import java.util.Map;
import java.util.Objects;
import java.util.function.Supplier;
import java.util.zip.Adler32;
import java.util.zip.CRC32;
import java.util.zip.CRC32C;
import java.util.zip.Checksum;

public class R21IntrinsicCorners {
    static int ITER = 100_000;
    static final int PASSES = 2;
    static int K; // per-pass call counter (reset by the runner), deterministic

    // ---------------------------------------------------------------- helpers
    static double bd(long bits) { return Double.longBitsToDouble(bits); }
    static float bf(int bits) { return Float.intBitsToFloat(bits); }
    static long raw(double d) { return Double.doubleToRawLongBits(d); }
    static long canon(double d) { return Double.doubleToLongBits(d); }
    static long rawf(float f) { return Float.floatToRawIntBits(f) & 0xFFFF_FFFFL; }
    static long canonf(float f) { return Float.floatToIntBits(f) & 0xFFFF_FFFFL; }
    static String ex(Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }

    static final double QNAN = bd(0x7ff8000000000000L);
    static final double PNAN = bd(0x7ff8ae0a00000000L);   // positive quiet NaN with a payload
    static final double NNAN = bd(0xfff8ae0a00000000L);   // negative quiet NaN with a payload
    static final double SNAN = bd(0x7ff0000000000001L);   // signalling
    static final double NSNAN = bd(0xfff00000dead0001L);  // negative signalling
    static final float QNANF = bf(0x7fc00000);
    static final float PNANF = bf(0x7fc8ae0a);
    static final float NNANF = bf(0xffc8ae0a);
    static final float SNANF = bf(0x7f800001);
    static final float NSNANF = bf(0xff80dead);

    interface Pass { String run(); }

    static void row(String name, Pass p) {
        String first = p.run();
        String last = first;
        for (int k = 1; k < PASSES; k++) {
            last = p.run();
        }
        System.out.println(name + " " + last);
        if (!last.equals(first)) {
            System.out.println("drift " + name + " pass0=" + first);
        }
    }

    // Own hashing / formatting: nothing under test is used to report.
    static long hashObj(Object o) {
        if (o == null) return 0x9E3779B97F4A7C15L;
        if (o instanceof Long) return (Long) o;
        if (o instanceof Integer) return (Integer) o;
        if (o instanceof Short) return 0x51L + (Short) o;
        if (o instanceof Byte) return 0xB1L + (Byte) o;
        if (o instanceof Boolean) return ((Boolean) o) ? 1231 : 1237;
        if (o instanceof Character) return 0xC0000L + (Character) o;
        if (o instanceof String) {
            String s = (String) o;
            long h = 7;
            for (int i = 0; i < s.length(); i++) h = h * 131 + s.charAt(i);
            return h;
        }
        long h = 17;
        if (o instanceof int[]) { for (int v : (int[]) o) h = h * 31 + v; return h; }
        if (o instanceof long[]) { for (long v : (long[]) o) h = h * 31 + v; return h; }
        if (o instanceof byte[]) { for (byte v : (byte[]) o) h = h * 31 + v; return h; }
        if (o instanceof char[]) { for (char v : (char[]) o) h = h * 31 + v; return h; }
        if (o instanceof short[]) { for (short v : (short[]) o) h = h * 31 + v; return h; }
        if (o instanceof boolean[]) { for (boolean v : (boolean[]) o) h = h * 31 + (v ? 1 : 2); return h; }
        if (o instanceof float[]) { for (float v : (float[]) o) h = h * 31 + rawf(v); return h; }
        if (o instanceof double[]) { for (double v : (double[]) o) h = h * 31 + raw(v); return h; }
        if (o instanceof Object[]) {
            for (Object v : (Object[]) o) h = h * 31 + hashObj(v);
            return h * 31 + hashObj(o.getClass().getName());
        }
        return 0x0BADL;
    }

    static String fmt(Object o) {
        if (o == null) return "null";
        if (o instanceof Long) return "0x" + Long.toHexString((Long) o);
        if (o instanceof Integer || o instanceof Short || o instanceof Byte || o instanceof Boolean) return String.valueOf(o);
        if (o instanceof Character) return "c" + (int) (Character) o;
        if (o instanceof String) return "\"" + o + "\"";
        StringBuilder b = new StringBuilder("{");
        if (o instanceof int[]) { int[] a = (int[]) o; for (int i = 0; i < a.length; i++) b.append(i == 0 ? "" : ",").append(a[i]); }
        else if (o instanceof long[]) { long[] a = (long[]) o; for (int i = 0; i < a.length; i++) b.append(i == 0 ? "" : ",").append(a[i]); }
        else if (o instanceof byte[]) { byte[] a = (byte[]) o; for (int i = 0; i < a.length; i++) b.append(i == 0 ? "" : ",").append(a[i]); }
        else if (o instanceof char[]) { char[] a = (char[]) o; for (int i = 0; i < a.length; i++) b.append(i == 0 ? "" : ",").append((int) a[i]); }
        else if (o instanceof short[]) { short[] a = (short[]) o; for (int i = 0; i < a.length; i++) b.append(i == 0 ? "" : ",").append(a[i]); }
        else if (o instanceof boolean[]) { boolean[] a = (boolean[]) o; for (int i = 0; i < a.length; i++) b.append(i == 0 ? "" : ",").append(a[i]); }
        else if (o instanceof float[]) { float[] a = (float[]) o; for (int i = 0; i < a.length; i++) b.append(i == 0 ? "" : ",").append(Integer.toHexString(Float.floatToRawIntBits(a[i]))); }
        else if (o instanceof double[]) { double[] a = (double[]) o; for (int i = 0; i < a.length; i++) b.append(i == 0 ? "" : ",").append(Long.toHexString(raw(a[i]))); }
        else if (o instanceof Object[]) {
            Object[] a = (Object[]) o;
            b.insert(0, o.getClass().getName());
            for (int i = 0; i < a.length; i++) b.append(i == 0 ? "" : ",").append(fmt(a[i]));
        } else {
            return "?" + o.getClass().getName();
        }
        return b.append('}').toString();
    }

    // The one runner. `dense`: corner j = i % n on every call (numeric families). Otherwise every
    // 16th call takes corner (i >>> 4) % n and the rest the family's plain input (j = -1), so a
    // throwing corner costs 1/16 of the calls.
    static String runList(int fam, int op, int n, boolean dense) {
        K = 0;
        Object[] last = new Object[n];
        Object plain = null;
        long h = 0;
        for (int i = 0; i < ITER; i++) {
            int j = dense ? i % n : (((i & 15) == 15) ? (i >>> 4) % n : -1);
            Object r;
            try {
                r = lst(fam, op, j);
            } catch (Throwable t) {
                r = "!" + ex(t);
            }
            K++;
            if (j >= 0) last[j] = r; else plain = r;
            h = h * 31 + hashObj(r);
        }
        StringBuilder sb = new StringBuilder();
        for (int j = 0; j < n; j++) {
            if (j > 0) sb.append(' ');
            Object r = last[j];
            sb.append(r instanceof String && ((String) r).startsWith("!") ? (String) r : fmt(r));
        }
        if (!dense) sb.append(" plain=").append(plain instanceof String && ((String) plain).startsWith("!") ? (String) plain : fmt(plain));
        return sb.append(" h=").append(Long.toHexString(h)).toString();
    }

    static final int F_D1 = 1, F_DS = 2, F_D2 = 3, F_D3 = 4, F_F1 = 5, F_FS = 6, F_F2 = 7, F_I2 = 8,
            F_L2 = 9, F_IB = 10, F_LB = 11, F_CH = 12, F_DT = 13, F_PA = 14,
            S_AC = 20, S_AR = 21, S_OB = 22, S_ST = 23, S_SB = 24, S_NU = 25, S_CR = 26, S_B6 = 27;

    static Object lst(int fam, int op, int j) {
        switch (fam) {
            case F_D1: return fD1(op, j);
            case F_DS: return fDS(op, j);
            case F_D2: return fD2(op, j);
            case F_D3: return fD3(op, j);
            case F_F1: return fF1(op, j);
            case F_FS: return fFS(op, j);
            case F_F2: return fF2(op, j);
            case F_I2: return fI2(op, j);
            case F_L2: return fL2(op, j);
            case F_IB: return fIB(op, j);
            case F_LB: return fLB(op, j);
            case F_CH: return fCH(op, j);
            case F_DT: return fDT(op, j);
            case F_PA: return fPA(op, j);
            case S_AC: return ac(op, j >= 0);
            case S_AR: return ar(op, j >= 0);
            case S_OB: return ob(op, j >= 0);
            case S_ST: return st(op, j >= 0);
            case S_SB: return sb(op, j >= 0);
            case S_NU: return nu(op, j >= 0);
            case S_CR: return cr(op, j >= 0);
            case S_B6: return b6(op, j >= 0);
            default: throw new IllegalStateException("family " + fam);
        }
    }

    // ---------------------------------------------------------------- math: unary double
    static final double[] D_M = {
        -0.0, 0.0, -0.5, 0.5, -1.5, 1.5, 2.5, -2.5, 0.49999999999999994, -0.49999999999999994,
        Double.MIN_VALUE, -Double.MIN_VALUE, Double.MIN_NORMAL, Double.MAX_VALUE, -Double.MAX_VALUE,
        Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, 4503599627370495.5, -4503599627370497.0,
        9007199254740993.0, 1e300, -1e-300, 0.1, -7.75, 123456.789, 3.0E-5, QNAN, PNAN,
    };
    static final double[] D_S = concat(D_M, new double[] {SNAN, NSNAN, NNAN, bd(0xfff8000000000000L)});
    static final String[] D1_OPS = {"floor", "ceil", "rint", "abs", "sqrt", "signum", "nextUp", "nextDown",
        "ulp", "toRadians", "toDegrees", "getExponent", "round", "d2l", "d2i", "doubleToLongBits", "d2f2d", "negate"};

    static double[] concat(double[] a, double[] b) {
        double[] r = new double[a.length + b.length];
        for (int i = 0; i < a.length; i++) r[i] = a[i];
        for (int i = 0; i < b.length; i++) r[a.length + i] = b[i];
        return r;
    }

    static long fD1(int op, int j) {
        double x = D_M[j];
        switch (op) {
            case 0: return raw(Math.floor(x));
            case 1: return raw(Math.ceil(x));
            case 2: return raw(Math.rint(x));
            case 3: return raw(Math.abs(x));
            case 4: return canon(Math.sqrt(x));
            case 5: return raw(Math.signum(x));
            case 6: return raw(Math.nextUp(x));
            case 7: return raw(Math.nextDown(x));
            case 8: return raw(Math.ulp(x));
            case 9: return raw(Math.toRadians(x));
            case 10: return raw(Math.toDegrees(x));
            case 11: return Math.getExponent(x);
            case 12: return Math.round(x);
            case 13: return (long) x;
            case 14: return (int) x;
            case 15: return Double.doubleToLongBits(x);
            case 16: return raw((double) (float) x);
            case 17: return raw(-x);
            default: throw new IllegalStateException();
        }
    }

    static final String[] DS_OPS = {"floor", "ceil", "rint", "abs", "sqrt", "signum", "nextUp", "nextDown",
        "ulp", "round", "getExponent", "copySignNeg", "toRadians"};

    static long fDS(int op, int j) {
        double x = D_S[j];
        switch (op) {
            case 0: return raw(StrictMath.floor(x));
            case 1: return raw(StrictMath.ceil(x));
            case 2: return raw(StrictMath.rint(x));
            case 3: return raw(StrictMath.abs(x));
            case 4: return raw(StrictMath.sqrt(x));
            case 5: return raw(StrictMath.signum(x));
            case 6: return raw(StrictMath.nextUp(x));
            case 7: return raw(StrictMath.nextDown(x));
            case 8: return raw(StrictMath.ulp(x));
            case 9: return StrictMath.round(x);
            case 10: return StrictMath.getExponent(x);
            case 11: return raw(StrictMath.copySign(x, -1.0));
            case 12: return raw(StrictMath.toRadians(x));
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- math: binary double
    static final double[] D2A = {0.0, -0.0, 1.0, QNAN, 1.0, NNAN, -0.0, Double.MIN_VALUE, Double.POSITIVE_INFINITY,
        3.0, 5.5, -5.5, 1e308, 2.0, -2.0, 1.0, 0.0, -1.0, Double.MAX_VALUE, 1e-320, 7.0, -7.0};
    static final double[] D2B = {-0.0, 0.0, QNAN, 1.0, NNAN, 1.0, -0.0, -Double.MIN_VALUE, Double.NEGATIVE_INFINITY,
        -0.0, 2.0, 2.0, 1e-308, Double.NEGATIVE_INFINITY, Double.POSITIVE_INFINITY, bd(0x3ff0000000000001L), 0.0,
        -0.0, Double.MAX_VALUE, -1e-320, -2.0, 2.0};
    static final String[] D2_OPS = {"min", "max", "copySign", "strictCopySign", "nextAfter", "IEEEremainder",
        "strictMin", "strictMax", "hypot", "atan2", "Double.compare", "Double.min", "Double.max", "scalb", "drem",
        "Double.equals"};

    static long fD2(int op, int j) {
        double a = D2A[j], b = D2B[j];
        switch (op) {
            case 0: return raw(Math.min(a, b));
            case 1: return raw(Math.max(a, b));
            case 2: return raw(Math.copySign(a, b));
            case 3: return raw(StrictMath.copySign(a, b));
            case 4: return raw(Math.nextAfter(a, b));
            case 5: return canon(Math.IEEEremainder(a, b));
            case 6: return raw(StrictMath.min(a, b));
            case 7: return raw(StrictMath.max(a, b));
            case 8: return canon(Math.hypot(a, b));
            case 9: return canon(Math.atan2(a, b));
            case 10: return Double.compare(a, b);
            case 11: return raw(Double.min(a, b));
            case 12: return raw(Double.max(a, b));
            case 13: return raw(Math.scalb(a, (int) b));
            case 14: return canon(a % b);
            case 15: return Double.valueOf(a).equals(b) ? 1 : 0;
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- math: fma
    static final double[] FMA_A = {0.1, bd(0x3ff0000000000001L), -0.0, 0.0, Double.POSITIVE_INFINITY, Double.MAX_VALUE,
        Double.MIN_VALUE, QNAN, 2.0, 1e308, -1.0, 0.5};
    static final double[] FMA_B = {10.0, bd(0x3fefffffffffffffL), 0.0, 0.0, 0.0, 2.0, 0.5, 1.0, 3.0, 10.0,
        Double.MIN_VALUE, Double.MIN_VALUE};
    static final double[] FMA_C = {-1.0, -1.0, 0.0, -0.0, 1.0, Double.NEGATIVE_INFINITY, 0.0, 1.0, QNAN,
        Double.NEGATIVE_INFINITY, 0.0, -0.0};
    static final float[] FMAF_A = {0.1f, bf(0x3f800001), -0f, 0f, Float.POSITIVE_INFINITY, Float.MAX_VALUE,
        Float.MIN_VALUE, QNANF, 2f, 1e38f, -1f, 0.5f};
    static final float[] FMAF_B = {10f, bf(0x3f7fffff), 0f, 0f, 0f, 2f, 0.5f, 1f, 3f, 10f, Float.MIN_VALUE,
        Float.MIN_VALUE};
    static final float[] FMAF_C = {-1f, -1f, 0f, -0f, 1f, Float.NEGATIVE_INFINITY, 0f, 1f, QNANF,
        Float.NEGATIVE_INFINITY, 0f, -0f};
    static final String[] D3_OPS = {"fmaD", "strictFmaD", "fmaF", "strictFmaF"};

    static long fD3(int op, int j) {
        switch (op) {
            case 0: return canon(Math.fma(FMA_A[j], FMA_B[j], FMA_C[j]));
            case 1: return canon(StrictMath.fma(FMA_A[j], FMA_B[j], FMA_C[j]));
            case 2: return canonf(Math.fma(FMAF_A[j], FMAF_B[j], FMAF_C[j]));
            case 3: return canonf(StrictMath.fma(FMAF_A[j], FMAF_B[j], FMAF_C[j]));
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- math: float
    static final float[] F_M = {-0f, 0f, -0.5f, 0.5f, 2.5f, -2.5f, 0.49999997f, Float.MIN_VALUE, -Float.MIN_VALUE,
        Float.MIN_NORMAL, Float.MAX_VALUE, -Float.MAX_VALUE, Float.POSITIVE_INFINITY, Float.NEGATIVE_INFINITY,
        8388607.5f, QNANF, PNANF, 1e10f, 0.1f, 16777217f, -3.75f, 65504f, 65520f, 6.1035156E-5f, 5.9604645E-8f};
    static final float[] F_S = concatf(F_M, new float[] {SNANF, NSNANF, NNANF});
    static final String[] F1_OPS = {"abs", "signum", "ulp", "nextUp", "nextDown", "round", "getExponent",
        "floatToIntBits", "floatToRawIntBits", "f2i", "f2l", "f2d", "sqrtAsDouble", "floatToFloat16",
        "float16RoundTrip", "copySignNeg"};

    static float[] concatf(float[] a, float[] b) {
        float[] r = new float[a.length + b.length];
        for (int i = 0; i < a.length; i++) r[i] = a[i];
        for (int i = 0; i < b.length; i++) r[a.length + i] = b[i];
        return r;
    }

    static long fF1(int op, int j) {
        float x = F_M[j];
        switch (op) {
            case 0: return rawf(Math.abs(x));
            case 1: return rawf(Math.signum(x));
            case 2: return rawf(Math.ulp(x));
            case 3: return rawf(Math.nextUp(x));
            case 4: return rawf(Math.nextDown(x));
            case 5: return Math.round(x);
            case 6: return Math.getExponent(x);
            case 7: return Float.floatToIntBits(x);
            case 8: return Float.floatToRawIntBits(x);
            case 9: return (int) x;
            case 10: return (long) x;
            case 11: return raw((double) x);
            case 12: return canon(Math.sqrt(x));
            case 13: return Float.floatToFloat16(x);
            case 14: return rawf(Float.float16ToFloat(Float.floatToFloat16(x)));
            case 15: return rawf(Math.copySign(x, -2f));
            default: throw new IllegalStateException();
        }
    }

    static final String[] FS_OPS = {"abs", "signum", "ulp", "nextUp", "nextDown", "round", "getExponent",
        "floatToIntBits", "floatToRawIntBits", "f2d", "copySignNeg"};

    static long fFS(int op, int j) {
        float x = F_S[j];
        switch (op) {
            case 0: return rawf(StrictMath.abs(x));
            case 1: return rawf(StrictMath.signum(x));
            case 2: return rawf(StrictMath.ulp(x));
            case 3: return rawf(StrictMath.nextUp(x));
            case 4: return rawf(StrictMath.nextDown(x));
            case 5: return StrictMath.round(x);
            case 6: return StrictMath.getExponent(x);
            case 7: return Float.floatToIntBits(x);
            case 8: return Float.floatToRawIntBits(x);
            case 9: return canon((double) x);
            case 10: return rawf(StrictMath.copySign(x, -2f));
            default: throw new IllegalStateException();
        }
    }

    static final float[] F2A = {0f, -0f, 1f, QNANF, 1f, NNANF, -0f, Float.MIN_VALUE, Float.POSITIVE_INFINITY, 3f,
        5.5f, -5.5f, 2f, 1f, -7f};
    static final float[] F2B = {-0f, 0f, QNANF, 1f, NNANF, 1f, -0f, -Float.MIN_VALUE, Float.NEGATIVE_INFINITY, -0f,
        2f, 2f, Float.NEGATIVE_INFINITY, bf(0x3f800001), 2f};
    static final String[] F2_OPS = {"min", "max", "copySign", "strictCopySign", "nextAfter", "Float.compare",
        "frem", "scalb", "Float.min", "Float.max"};

    static long fF2(int op, int j) {
        float a = F2A[j], b = F2B[j];
        switch (op) {
            case 0: return rawf(Math.min(a, b));
            case 1: return rawf(Math.max(a, b));
            case 2: return rawf(Math.copySign(a, b));
            case 3: return rawf(StrictMath.copySign(a, b));
            case 4: return rawf(Math.nextAfter(a, (double) b));
            case 5: return Float.compare(a, b);
            case 6: return canonf(a % b);
            case 7: return rawf(Math.scalb(a, (int) b));
            case 8: return rawf(Float.min(a, b));
            case 9: return rawf(Float.max(a, b));
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- math: int / long exact
    static final int MIN = Integer.MIN_VALUE, MAX = Integer.MAX_VALUE;
    static final int[] I2A = {MAX, MIN, MIN, 7, -7, -7, 7, 0, 46341, MIN, 5, -1, 1 << 16, -46341, 3, MAX, MIN};
    static final int[] I2B = {1, -1, 1, -2, 2, -2, 2, -5, 46341, 0, 0, MIN, 1 << 15, 46341, MAX, MAX, MIN};
    static final String[] I2_OPS = {"addExact", "subtractExact", "multiplyExact", "floorDiv", "floorMod", "ceilDiv",
        "ceilMod", "divideExact", "absExact", "incrementExact", "decrementExact", "negateExact", "divideUnsigned",
        "remainderUnsigned", "compareUnsigned", "compare", "min", "max", "abs", "clamp", "multiplyFull",
        "floorDivExact", "ceilDivExact", "idiv", "irem"};

    static long fI2(int op, int j) {
        int a = I2A[j], b = I2B[j];
        switch (op) {
            case 0: return Math.addExact(a, b);
            case 1: return Math.subtractExact(a, b);
            case 2: return Math.multiplyExact(a, b);
            case 3: return Math.floorDiv(a, b);
            case 4: return Math.floorMod(a, b);
            case 5: return Math.ceilDiv(a, b);
            case 6: return Math.ceilMod(a, b);
            case 7: return Math.divideExact(a, b);
            case 8: return Math.absExact(a);
            case 9: return Math.incrementExact(a);
            case 10: return Math.decrementExact(a);
            case 11: return Math.negateExact(a);
            case 12: return Integer.divideUnsigned(a, b);
            case 13: return Integer.remainderUnsigned(a, b);
            case 14: return Integer.compareUnsigned(a, b);
            case 15: return Integer.compare(a, b);
            case 16: return Math.min(a, b);
            case 17: return Math.max(a, b);
            case 18: return Math.abs(a);
            case 19: return Math.clamp((long) a, b, b + 10);
            case 20: return Math.multiplyFull(a, b);
            case 21: return Math.floorDivExact(a, b);
            case 22: return Math.ceilDivExact(a, b);
            case 23: return a / b;
            case 24: return a % b;
            default: throw new IllegalStateException();
        }
    }

    static final long LMIN = Long.MIN_VALUE, LMAX = Long.MAX_VALUE;
    static final long[] L2A = {LMAX, LMIN, LMIN, 7, -7, -7, 0, 3037000500L, LMIN, 5, -1, 1L << 32, 0x7fffffffL,
        Integer.MIN_VALUE, LMAX, 0x123456789L, -0x123456789L};
    static final long[] L2B = {1, -1, 1, -2, 2, -2, -5, 3037000500L, 0, 0, LMIN, 1L << 31, 2, -1, LMAX,
        0x1_0000_0000L, 3};
    static final String[] L2_OPS = {"addExact", "subtractExact", "multiplyExact", "floorDiv", "floorMod", "ceilDiv",
        "ceilMod", "divideExact", "absExact", "incrementExact", "decrementExact", "negateExact", "divideUnsigned",
        "remainderUnsigned", "compareUnsigned", "compare", "min", "max", "abs", "multiplyExactLI", "floorDivLI",
        "floorModLI", "multiplyHigh", "unsignedMultiplyHigh", "toIntExact", "clamp", "floorDivExact", "ldiv", "lrem",
        "ceilDivLI"};

    static long fL2(int op, int j) {
        long a = L2A[j], b = L2B[j];
        switch (op) {
            case 0: return Math.addExact(a, b);
            case 1: return Math.subtractExact(a, b);
            case 2: return Math.multiplyExact(a, b);
            case 3: return Math.floorDiv(a, b);
            case 4: return Math.floorMod(a, b);
            case 5: return Math.ceilDiv(a, b);
            case 6: return Math.ceilMod(a, b);
            case 7: return Math.divideExact(a, b);
            case 8: return Math.absExact(a);
            case 9: return Math.incrementExact(a);
            case 10: return Math.decrementExact(a);
            case 11: return Math.negateExact(a);
            case 12: return Long.divideUnsigned(a, b);
            case 13: return Long.remainderUnsigned(a, b);
            case 14: return Long.compareUnsigned(a, b);
            case 15: return Long.compare(a, b);
            case 16: return Math.min(a, b);
            case 17: return Math.max(a, b);
            case 18: return Math.abs(a);
            case 19: return Math.multiplyExact(a, (int) b);
            case 20: return Math.floorDiv(a, (int) b);
            case 21: return Math.floorMod(a, (int) b);
            case 22: return Math.multiplyHigh(a, b);
            case 23: return Math.unsignedMultiplyHigh(a, b);
            case 24: return Math.toIntExact(a);
            case 25: return Math.clamp(a, b, b + 10);
            case 26: return Math.floorDivExact(a, b);
            case 27: return a / b;
            case 28: return a % b;
            case 29: return Math.ceilDiv(a, (int) b);
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- bits
    static final int[] I_BITS = {0, 1, -1, MIN, MAX, 0x80, 0x00F00000, 0x12345678, 0xCAFEBABE, -0x12345678, 2, 3,
        0x7F, 0x8000};
    static final int[] DIST = {-1, 0, 1, 31, 32, 33, -33, 63, 64, 5, -5, 100, MIN, MAX};
    static final int[] MASKS = {0xFF00FFF0, 0, -1, 0x80000001, 0x0F0F0F0F, 0x55555555, 1, MIN, 0x7FFFFFFF,
        0x00FF0000, 0xAAAAAAAA, 3, 0xF0000000, 0x0000F00F};
    static final String[] IB_OPS = {"numberOfLeadingZeros", "numberOfTrailingZeros", "bitCount", "highestOneBit",
        "lowestOneBit", "reverse", "reverseBytes", "signum", "rotateLeft", "rotateRight", "compress", "expand",
        "toUnsignedLong", "Short.reverseBytes", "Character.reverseBytes", "toUnsignedString"};

    static Object fIB(int op, int j) {
        int x = I_BITS[j];
        switch (op) {
            case 0: return Integer.numberOfLeadingZeros(x);
            case 1: return Integer.numberOfTrailingZeros(x);
            case 2: return Integer.bitCount(x);
            case 3: return Integer.highestOneBit(x);
            case 4: return Integer.lowestOneBit(x);
            case 5: return Integer.reverse(x);
            case 6: return Integer.reverseBytes(x);
            case 7: return Integer.signum(x);
            case 8: return Integer.rotateLeft(x, DIST[j % DIST.length]);
            case 9: return Integer.rotateRight(x, DIST[j % DIST.length]);
            case 10: return Integer.compress(x, MASKS[j % MASKS.length]);
            case 11: return Integer.expand(x, MASKS[j % MASKS.length]);
            case 12: return Integer.toUnsignedLong(x);
            case 13: return (int) Short.reverseBytes((short) x);
            case 14: return (int) Character.reverseBytes((char) x);
            case 15: return Integer.toUnsignedString(x);
            default: throw new IllegalStateException();
        }
    }

    static final long[] L_BITS = {0, 1, -1, LMIN, LMAX, 0x80, 1L << 32, 0x123456789ABCDEF0L, 0xCAFEBABEDEADBEEFL,
        -0x123456789L, 0xFFFFFFFFL, 0x8000000000000001L};
    static final long[] LMASKS = {0xFF00FF00FF00FF00L, 0, -1, 0x8000000000000001L, 0x0F0F0F0F0F0F0F0FL,
        0x5555555555555555L, 1, LMIN, LMAX, 0xFFFFFFFFL, 0xAAAAAAAAAAAAAAAAL, 3};
    static final String[] LB_OPS = {"numberOfLeadingZeros", "numberOfTrailingZeros", "bitCount", "highestOneBit",
        "lowestOneBit", "reverse", "reverseBytes", "signum", "rotateLeft", "rotateRight", "compress", "expand",
        "hashCode", "toUnsignedString"};

    static Object fLB(int op, int j) {
        long x = L_BITS[j];
        switch (op) {
            case 0: return Long.numberOfLeadingZeros(x);
            case 1: return Long.numberOfTrailingZeros(x);
            case 2: return Long.bitCount(x);
            case 3: return Long.highestOneBit(x);
            case 4: return Long.lowestOneBit(x);
            case 5: return Long.reverse(x);
            case 6: return Long.reverseBytes(x);
            case 7: return Long.signum(x);
            case 8: return Long.rotateLeft(x, DIST[j % DIST.length]);
            case 9: return Long.rotateRight(x, DIST[j % DIST.length]);
            case 10: return Long.compress(x, LMASKS[j % LMASKS.length]);
            case 11: return Long.expand(x, LMASKS[j % LMASKS.length]);
            case 12: return Long.hashCode(x);
            case 13: return Long.toUnsignedString(x);
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- Character
    static final int[] CPS = {'0', '9', 'a', 'Z', '_', '$', 0x0660, 0x06F0, 0x0966, 0xFF10, 0xFF21, 0x1D7CE,
        0x1D7FF, 0x10400, 0x10428, 0x1D400, 0x1F600, 0x00A0, 0x2007, 0x202F, 0x0085, 0x001C, 0x0009, 0xDFFF, 0xD800,
        0x10FFFF, 0x110000, -1, 0x0130, 0x0131, 0x00DF, 0x1E9E, 0x216B, 0x0BF1, 0x3007, 0x00B2, 0x01C5, 0x0345,
        0x00AD, 0x200B, 0xFEFF, 0x1F1E6, 0x20000, 0xE0001, 0x0000, 0x10FFFE, 0x1F3FB};
    static final String[] CH_OPS = {"isDigit", "isLetter", "isLetterOrDigit", "isAlphabetic", "isUpperCase",
        "isLowerCase", "isTitleCase", "isWhitespace", "isSpaceChar", "isJavaIdentifierStart", "isJavaIdentifierPart",
        "isIdentifierIgnorable", "getType", "digit10", "digit36", "getNumericValue", "toUpperCase", "toLowerCase",
        "toTitleCase", "isValidCodePoint", "isBmpCodePoint", "isSupplementaryCodePoint", "charCount", "isMirrored",
        "isIdeographic", "isEmoji", "isDefined", "isISOControl", "isDigitC", "isLetterC", "toUpperCaseC",
        "toLowerCaseC", "isHighSurrogateC", "isLowSurrogateC", "isSurrogateC", "getDirectionality",
        "isUnicodeIdentifierStart", "isUnicodeIdentifierPart", "isEmojiPresentation", "toCharsLength", "toStringLen",
        "highSurrogate", "lowSurrogate", "isEmojiModifierBase", "isExtendedPictographic"};

    static Object fCH(int op, int j) {
        int cp = CPS[j];
        char c = (char) cp;
        switch (op) {
            case 0: return Character.isDigit(cp);
            case 1: return Character.isLetter(cp);
            case 2: return Character.isLetterOrDigit(cp);
            case 3: return Character.isAlphabetic(cp);
            case 4: return Character.isUpperCase(cp);
            case 5: return Character.isLowerCase(cp);
            case 6: return Character.isTitleCase(cp);
            case 7: return Character.isWhitespace(cp);
            case 8: return Character.isSpaceChar(cp);
            case 9: return Character.isJavaIdentifierStart(cp);
            case 10: return Character.isJavaIdentifierPart(cp);
            case 11: return Character.isIdentifierIgnorable(cp);
            case 12: return Character.getType(cp);
            case 13: return Character.digit(cp, 10);
            case 14: return Character.digit(cp, 36);
            case 15: return Character.getNumericValue(cp);
            case 16: return Character.toUpperCase(cp);
            case 17: return Character.toLowerCase(cp);
            case 18: return Character.toTitleCase(cp);
            case 19: return Character.isValidCodePoint(cp);
            case 20: return Character.isBmpCodePoint(cp);
            case 21: return Character.isSupplementaryCodePoint(cp);
            case 22: return Character.charCount(cp);
            case 23: return Character.isMirrored(cp);
            case 24: return Character.isIdeographic(cp);
            case 25: return Character.isEmoji(cp);
            case 26: return Character.isDefined(cp);
            case 27: return Character.isISOControl(cp);
            case 28: return Character.isDigit(c);
            case 29: return Character.isLetter(c);
            case 30: return (int) Character.toUpperCase(c);
            case 31: return (int) Character.toLowerCase(c);
            case 32: return Character.isHighSurrogate(c);
            case 33: return Character.isLowSurrogate(c);
            case 34: return Character.isSurrogate(c);
            case 35: return (int) Character.getDirectionality(cp);
            case 36: return Character.isUnicodeIdentifierStart(cp);
            case 37: return Character.isUnicodeIdentifierPart(cp);
            case 38: return Character.isEmojiPresentation(cp);
            case 39: return Character.toChars(cp).length;
            case 40: return Character.toString(cp).length();
            case 41: return (int) Character.highSurrogate(cp);
            case 42: return (int) Character.lowSurrogate(cp);
            case 43: return Character.isEmojiModifierBase(cp);
            case 44: return Character.isExtendedPictographic(cp);
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- toString, fixed lists
    static final double[] D_TOSTR = {1.0E23, 2.0E23, 8.41E21, 5.0E-324, 1.0E-323, Double.MAX_VALUE,
        Double.MIN_NORMAL, bd(0x000fffffffffffffL), 1.0E-5, 0.001, 1.0E7, 9999999.999999998, 0.1 + 0.2, 100.0,
        1.0E16, 1.23456789012345678E17, bd(0x430999999999999aL), bd(0x3e70000000000000L), 1.0 / 3, 2.0 / 3, 1e21,
        1e22, 1e-7, 123456.0, 1234567.0, 12345678.0, -0.0, 0.0, Double.NaN, Double.POSITIVE_INFINITY,
        Double.NEGATIVE_INFINITY, 4.35, 0.3, 1.1, 2.0E-3, 9.999999999999999E22, 5.0E-310, 1.0E-320, Math.PI, Math.E,
        299792458.0, 6.02214076E23, 1.602176634E-19, 9.223372036854776E18, 9007199254740994.0, 4.9E-322,
        1.0E-322, 2.2250738585072014E-308, 2.225073858507201E-308, 1.0E-3, 9.999999999999998E-4, 1.0E7 - 1e-9,
        5.551115123125783E-17, 1.9999999999999998, 0.30000000000000004, 1.0E-300, -1.5E300};
    static final float[] F_TOSTR = {1.0E10f, 3.4028235E38f, 1.4E-45f, 1.17549435E-38f, 0.1f, 1.0E-5f, 3.3554432E7f,
        8.589973E9f, 1.0E23f, 2.0E-3f, 1.0f / 3, 2.0f / 3, 100f, 1.0E7f, 9999999f, 16777217f, 0.3f, 1.1f, 4.35f,
        2.8E-45f, 1.0E-40f, Float.MIN_NORMAL, -0f, Float.NaN, Float.POSITIVE_INFINITY, 1.0E-3f, 9.999999E-4f,
        1.234567E8f, 7.038531E-26f, 3.4E38f, 1.00000005f, 9.9999998E10f, 2.7182817f, 3.1415927f, 0.3333333f};
    static final String[] DT_OPS = {"Double.toString", "String.valueOfD", "concatD", "sbAppendD", "parseRoundTripD",
        "toHexString", "Float.toString", "String.valueOfF", "concatF", "parseRoundTripF"};
    static final StringBuilder DT_SB = new StringBuilder();

    static Object fDT(int op, int j) {
        switch (op) {
            case 0: return Double.toString(D_TOSTR[j]);
            case 1: return String.valueOf(D_TOSTR[j]);
            case 2: return "" + D_TOSTR[j];
            case 3: DT_SB.setLength(0); return DT_SB.append(D_TOSTR[j]).toString();
            case 4: return raw(Double.parseDouble(Double.toString(D_TOSTR[j]))) == raw(D_TOSTR[j]) || Double.isNaN(D_TOSTR[j]);
            case 5: return Double.toHexString(D_TOSTR[j]);
            case 6: return Float.toString(F_TOSTR[j]);
            case 7: return String.valueOf(F_TOSTR[j]);
            case 8: return "" + F_TOSTR[j];
            case 9: return Float.floatToRawIntBits(Float.parseFloat(Float.toString(F_TOSTR[j]))) == Float.floatToRawIntBits(F_TOSTR[j]) || Float.isNaN(F_TOSTR[j]);
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- parse
    static final String[] P_STR = {"2147483647", "2147483648", "-2147483648", "-2147483649", "", null, "-", "+", "+5",
        "1_000", " 1", "٣", "ff", "FF", "-80000000", "80000000", "1", "1", "z", "Z", "0x10", "１２",
        "00000000000000000000000000012", "9223372036854775807", "9223372036854775808", "-9223372036854775808",
        "4294967295", "4294967296", "-1", "0x80000000", "-0x80000000", "#ff", "010", "1e3", "NaN", "-Infinity",
        "0x1p3", "1.0f", "  2.5  ", "1d", ".5", "5.", "+.5e-3", "0x1.8p1", "128", "-129", "32768", "-32769", "-0",
        "7fffffff", "٠١"};
    static final int[] P_RAD = {10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 16, 16, 16, 16, 37, 1, 36, 36, 16,
        10, 10, 10, 10, 10, 10, 10, 10, 16, 16, 16, 8, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
        16, 10};
    static final String[] PA_OPS = {"Integer.parseInt", "Integer.parseIntRadix", "Long.parseLongRadix",
        "Long.parseLong", "Integer.valueOfRadix", "Integer.parseUnsignedInt", "Short.parseShort", "Byte.parseByte",
        "Integer.decode", "Long.parseUnsignedLong", "Double.parseDouble", "Float.parseFloat", "Integer.parseIntCS",
        "Long.decode"};

    static Object fPA(int op, int j) {
        String s = j < 0 ? "123" : P_STR[j];
        int r = j < 0 ? 10 : P_RAD[j];
        switch (op) {
            case 0: return Integer.parseInt(s);
            case 1: return Integer.parseInt(s, r);
            case 2: return Long.parseLong(s, r);
            case 3: return Long.parseLong(s);
            case 4: return Integer.valueOf(s, r);
            case 5: return Integer.parseUnsignedInt(s, r);
            case 6: return Short.parseShort(s, r);
            case 7: return Byte.parseByte(s, r);
            case 8: return Integer.decode(s);
            case 9: return Long.parseUnsignedLong(s, r);
            case 10: return raw(Double.parseDouble(s));
            case 11: return rawf(Float.parseFloat(s));
            case 12: return Integer.parseInt(s, 0, s.length(), r);
            case 13: return Long.decode(s);
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- System.arraycopy scenarios
    static int[] seqI(int n) { int[] a = new int[n]; for (int i = 0; i < n; i++) a[i] = i + 1; return a; }
    static long[] seqL(int n) { long[] a = new long[n]; for (int i = 0; i < n; i++) a[i] = (i + 1) * 0x100000001L; return a; }
    static byte[] seqB(int n) { byte[] a = new byte[n]; for (int i = 0; i < n; i++) a[i] = (byte) (i * 37 - 100); return a; }
    static char[] seqC(int n) { char[] a = new char[n]; for (int i = 0; i < n; i++) a[i] = (char) (0xFFF0 + i); return a; }
    static short[] seqS(int n) { short[] a = new short[n]; for (int i = 0; i < n; i++) a[i] = (short) (i * 5000 - 20000); return a; }

    static final String[] AC_ROWS = {"int-overlap-fwd", "int-overlap-back", "long-overlap-fwd", "byte-overlap-fwd",
        "char-disjoint", "short-overlap-back", "double-nan-bits", "boolean", "float-self", "string-self-fwd",
        "string-to-object", "object-to-number", "null-src", "null-dst", "negative-length", "negative-srcpos",
        "negative-dstpos", "last-source-index", "last-dest-index", "object-into-int", "int-into-long",
        "src-not-array", "dst-not-array", "element-mismatch-partial", "len0-srcpos-past-end", "srcpos-overflow",
        "int-into-integer", "number-element-mismatch", "len0-null-src", "dstpos-overflow", "float-into-double",
        "long-self-back-big", "char-self-fwd-unaligned"};

    static Object ac(int s, boolean bad) {
        switch (s) {
            case 0: { int[] a = seqI(10); System.arraycopy(a, 0, a, 2, 7); return a; }
            case 1: { int[] a = seqI(10); System.arraycopy(a, 2, a, 0, 7); return a; }
            case 2: { long[] a = seqL(10); System.arraycopy(a, 1, a, 3, 6); return a; }
            case 3: { byte[] a = seqB(10); System.arraycopy(a, 0, a, 1, 9); return a; }
            case 4: { char[] a = seqC(10); char[] d = new char[12]; System.arraycopy(a, 2, d, 5, 7); return d; }
            case 5: { short[] a = seqS(10); System.arraycopy(a, 3, a, 1, 6); return a; }
            case 6: {
                double[] x = {SNAN, NNAN, -0.0, 1.0};
                double[] y = new double[4];
                System.arraycopy(x, 0, y, 0, 4);
                return y;
            }
            case 7: { boolean[] t = {true, false, true}; boolean[] u = new boolean[5]; System.arraycopy(t, 0, u, 2, 3); return u; }
            case 8: { float[] f = {1f, SNANF, -0f, 4f}; System.arraycopy(f, 1, f, 0, 3); return f; }
            case 9: { String[] a = {"a", "b", "c", "d", "e"}; System.arraycopy(a, 0, a, 1, 4); return a; }
            case 10: { String[] a = {"a", "b", "c"}; Object[] o = new Object[4]; System.arraycopy(a, 0, o, 1, 3); return o; }
            case 11: { Object[] a = {1, 2L, 3.0f}; Number[] d = new Number[3]; System.arraycopy(a, 0, d, 0, 3); return d; }
            case 12: { int[] d = new int[5]; System.arraycopy(bad ? null : seqI(5), 0, d, 0, 3); return d; }
            case 13: { int[] a = seqI(5); System.arraycopy(a, 0, bad ? null : new int[5], 0, 3); return a; }
            case 14: { int[] a = seqI(5); int[] d = new int[5]; System.arraycopy(a, 0, d, 0, bad ? -1 : 3); return d; }
            case 15: { int[] a = seqI(5); int[] d = new int[5]; System.arraycopy(a, bad ? -1 : 1, d, 0, 3); return d; }
            case 16: { int[] a = seqI(5); int[] d = new int[5]; System.arraycopy(a, 0, d, bad ? -1 : 1, 3); return d; }
            case 17: { int[] a = seqI(10); int[] d = new int[10]; System.arraycopy(a, 5, d, 0, bad ? 6 : 5); return d; }
            case 18: { long[] a = seqL(10); long[] d = new long[10]; System.arraycopy(a, 0, d, 4, bad ? 8 : 6); return d; }
            case 19: { Object src = bad ? new Object[10] : (Object) seqI(10); int[] d = new int[10]; System.arraycopy(src, 0, d, 0, 3); return d; }
            case 20: { Object dst = bad ? new long[10] : (Object) new int[10]; System.arraycopy(seqI(10), 0, dst, 0, 3); return dst; }
            case 21: { Object src = bad ? "string" : (Object) seqI(4); int[] d = new int[4]; System.arraycopy(src, 0, d, 0, 2); return d; }
            case 22: { Object dst = bad ? "string" : (Object) new int[4]; System.arraycopy(seqI(4), 0, dst, 0, 2); return dst; }
            case 23: {
                Object[] src = bad ? new Object[] {"a", 1, "c"} : new Object[] {"a", "b", "c"};
                String[] d = new String[3];
                try {
                    System.arraycopy(src, 0, d, 0, 3);
                } catch (ArrayStoreException e) {
                    return "!" + ex(e) + " dst=" + fmt(d);
                }
                return d;
            }
            case 24: { int[] a = seqI(10); int[] d = new int[10]; System.arraycopy(a, bad ? 11 : 10, d, 0, 0); return d; }
            case 25: { int[] a = seqI(10); int[] d = new int[10]; System.arraycopy(a, bad ? Integer.MAX_VALUE : 0, d, 0, 1); return d; }
            case 26: { Object dst = bad ? new Integer[3] : (Object) new int[3]; System.arraycopy(seqI(3), 0, dst, 0, 3); return dst; }
            case 27: {
                Object[] src = bad ? new Object[] {1, "x", 3} : new Object[] {1, 2, 3};
                Number[] d = new Number[3];
                try {
                    System.arraycopy(src, 0, d, 0, 3);
                } catch (ArrayStoreException e) {
                    return "!" + ex(e) + " dst=" + fmt(d);
                }
                return d;
            }
            case 28: { int[] d = new int[3]; System.arraycopy(bad ? null : seqI(3), 0, d, 0, 0); return d; }
            case 29: { int[] d = new int[10]; System.arraycopy(seqI(10), 0, d, bad ? Integer.MAX_VALUE : 9, 1); return d; }
            case 30: { Object dst = bad ? new double[3] : (Object) new float[3]; System.arraycopy(new float[] {1f, 2f, 3f}, 0, dst, 0, 3); return dst; }
            case 31: { long[] a = seqL(300); System.arraycopy(a, 0, a, 7, 290); return a; }
            case 32: { char[] a = seqC(64); System.arraycopy(a, 1, a, 0, 63); return a; }
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- Arrays scenarios
    static final String[] AR_ROWS = {"copyOf-int-grow", "copyOf-int-shrink", "copyOf-negative", "copyOf-string",
        "copyOf-newType-number", "copyOf-newType-mismatch", "copyOfRange-int-past-end", "copyOfRange-from-gt-to",
        "copyOfRange-from-past-length", "copyOfRange-negative-from", "copyOfRange-byte", "copyOfRange-char",
        "copyOf-boolean-grow", "copyOf-double-nan", "equals-float-nan-payloads", "equals-float-signed-zero",
        "equals-double-nan-payloads", "equals-double-signed-zero", "equals-null-null", "equals-int-null",
        "equals-int-ranges", "equals-range-from-gt-to", "equals-range-past-end", "equals-byte-100",
        "equals-char", "equals-long", "equals-boolean", "equals-short", "fill-int", "fill-int-range",
        "fill-range-from-gt-to", "fill-range-negative", "fill-range-past-end", "fill-null", "fill-double-neg-zero",
        "fill-boolean", "fill-byte", "fill-char", "fill-long", "fill-object-store-check", "fill-float-nan",
        "hashCode-int", "hashCode-long", "hashCode-byte", "hashCode-char", "hashCode-short", "hashCode-float-nan",
        "hashCode-double-nan", "hashCode-boolean", "hashCode-object", "hashCode-null", "deepHashCode",
        "mismatch-int-1024", "mismatch-byte-37", "mismatch-long-prefix", "mismatch-float-nan", "mismatch-double-zero",
        "mismatch-int-ranges", "mismatch-char", "mismatch-equal", "mismatch-null", "mismatch-object",
        "compare-byte-signed", "compareUnsigned-byte", "compare-int-prefix", "compare-null", "sort-int",
        "sort-char", "sort-long", "sort-byte", "sort-short", "sort-double-nan-zero", "sort-float", "sort-int-range",
        "sort-range-bad", "equals-object-nulls", "mismatch-byte-long-tail", "equals-int-1000-last"};

    static Object ar(int s, boolean bad) {
        switch (s) {
            case 0: return Arrays.copyOf(seqI(3), 5);
            case 1: return Arrays.copyOf(seqI(3), 1);
            case 2: return Arrays.copyOf(seqI(3), bad ? -1 : 2);
            case 3: return Arrays.copyOf(new String[] {"x", "y"}, 4);
            case 4: return Arrays.copyOf(new Object[] {1, 2L, 3.5}, 2, Number[].class);
            case 5: {
                Class<? extends Object[]> nt = bad ? Integer[].class : String[].class;
                return Arrays.copyOf(new Object[] {"a"}, 1, nt);
            }
            case 6: return Arrays.copyOfRange(seqI(3), 1, 5);
            case 7: return Arrays.copyOfRange(seqI(3), bad ? 2 : 1, 1);
            case 8: return Arrays.copyOfRange(seqI(3), bad ? 4 : 3, 6);
            case 9: return Arrays.copyOfRange(seqI(3), bad ? -1 : 0, 2);
            case 10: return Arrays.copyOfRange(seqB(5), 1, 3);
            case 11: return Arrays.copyOfRange(seqC(3), 0, 5);
            case 12: return Arrays.copyOf(new boolean[] {true, true}, 4);
            case 13: return Arrays.copyOf(new double[] {SNAN, NNAN, -0.0}, 4);
            case 14: return Arrays.equals(new float[] {bf(0x7fc00001), 1f}, new float[] {bf(0x7fc00002), 1f});
            case 15: return Arrays.equals(new float[] {-0f}, new float[] {0f});
            case 16: return Arrays.equals(new double[] {bd(0x7ff8000000000001L)}, new double[] {NNAN});
            case 17: return Arrays.equals(new double[] {-0.0}, new double[] {0.0});
            case 18: return Arrays.equals((int[]) null, (int[]) null);
            case 19: return Arrays.equals(seqI(2), (int[]) null);
            case 20: return Arrays.equals(seqI(5), 0, 3, new int[] {9, 1, 2, 3}, 1, 4);
            case 21: return Arrays.equals(seqI(5), bad ? 3 : 1, 1, seqI(5), 0, 0);
            case 22: return Arrays.equals(seqI(10), 0, bad ? 11 : 10, seqI(10), 0, 10);
            case 23: { byte[] a = seqB(100); byte[] b = seqB(100); if (bad) b[99]++; return Arrays.equals(a, b); }
            case 24: return Arrays.equals(seqC(7), seqC(7));
            case 25: { long[] b = seqL(9); b[8] = 0; return Arrays.equals(seqL(9), b); }
            case 26: return Arrays.equals(new boolean[] {true, false}, new boolean[] {true, false});
            case 27: return Arrays.equals(seqS(6), seqS(6));
            case 28: { int[] a = new int[7]; Arrays.fill(a, 7); return a; }
            case 29: { int[] a = new int[5]; Arrays.fill(a, 1, 3, 9); return a; }
            case 30: { int[] a = new int[5]; Arrays.fill(a, bad ? 3 : 1, 1, 9); return a; }
            case 31: { int[] a = new int[5]; Arrays.fill(a, bad ? -1 : 0, 2, 9); return a; }
            case 32: { int[] a = new int[10]; Arrays.fill(a, 0, bad ? 11 : 10, 9); return a; }
            case 33: { int[] a = bad ? null : new int[2]; Arrays.fill(a, 1); return a; }
            case 34: { double[] a = new double[3]; Arrays.fill(a, -0.0); return a; }
            case 35: { boolean[] a = new boolean[3]; Arrays.fill(a, true); return a; }
            case 36: { byte[] a = new byte[5]; Arrays.fill(a, (byte) -1); return a; }
            case 37: { char[] a = new char[4]; Arrays.fill(a, 'Ā'); return a; }
            case 38: { long[] a = new long[3]; Arrays.fill(a, Long.MIN_VALUE); return a; }
            case 39: { Object[] a = new String[3]; Arrays.fill(a, bad ? (Object) Integer.valueOf(1) : "s"); return a; }
            case 40: { float[] a = new float[3]; Arrays.fill(a, NNANF); return a; }
            case 41: return Arrays.hashCode(new int[] {1, -1, MIN, MAX, 0});
            case 42: return Arrays.hashCode(new long[] {1, -1, LMIN, LMAX, 0});
            case 43: return Arrays.hashCode(seqB(33));
            case 44: return Arrays.hashCode(seqC(17));
            case 45: return Arrays.hashCode(seqS(19));
            case 46: return Arrays.hashCode(new float[] {bf(0x7fc00001), NNANF, -0f, 0f});
            case 47: return Arrays.hashCode(new double[] {bd(0x7ff8000000000001L), NNAN, -0.0, 0.0});
            case 48: return Arrays.hashCode(new boolean[] {true, false, true});
            case 49: return Arrays.hashCode(new Object[] {"a", null, 1, 2L});
            case 50: return Arrays.hashCode((int[]) null) + Arrays.hashCode((Object[]) null) * 3;
            case 51: return Arrays.deepHashCode(new Object[] {new int[] {1, 2}, new Object[] {"x", null}, null});
            case 52: { int[] a = seqI(1024); int[] b = seqI(1024); b[1000] = -1; return Arrays.mismatch(a, b); }
            case 53: { byte[] a = seqB(100); byte[] b = seqB(100); b[37] ^= 1; return Arrays.mismatch(a, b); }
            case 54: return Arrays.mismatch(seqL(5), seqL(9));
            case 55: return Arrays.mismatch(new float[] {bf(0x7fc00001), 1f}, new float[] {bf(0x7fc00003), 1f});
            case 56: return Arrays.mismatch(new double[] {-0.0, 1}, new double[] {0.0, 1});
            case 57: { int[] b = seqI(8); b[6] = 0; return Arrays.mismatch(seqI(8), 1, 5, b, 1, 5) * 100 + Arrays.mismatch(seqI(8), 2, 8, b, 2, 8); }
            case 58: { char[] b = seqC(40); b[39] = 'x'; return Arrays.mismatch(seqC(40), b); }
            case 59: return Arrays.mismatch(seqI(33), seqI(33));
            case 60: return Arrays.mismatch(bad ? null : seqI(2), seqI(2));
            case 61: return Arrays.mismatch(new Object[] {"a", "b", 3}, new Object[] {"a", new String("b"), 4});
            case 62: return Integer.signum(Arrays.compare(new byte[] {-1}, new byte[] {1}));
            case 63: return Integer.signum(Arrays.compareUnsigned(new byte[] {-1}, new byte[] {1}));
            case 64: return Arrays.compare(seqI(3), seqI(5));
            case 65: return Arrays.compare((int[]) null, seqI(1));
            case 66: { int[] a = {5, MIN, 3, MAX, 3, -1, 0, 5}; Arrays.sort(a); return a; }
            case 67: { char[] a = {'z', '￿', 'a', 'Ā', 'a'}; Arrays.sort(a); return a; }
            case 68: { long[] a = {5, LMIN, 3, LMAX, -1}; Arrays.sort(a); return a; }
            case 69: { byte[] a = {5, -128, 127, -1, 0}; Arrays.sort(a); return a; }
            case 70: { short[] a = {5, -32768, 32767, -1, 0}; Arrays.sort(a); return a; }
            case 71: { double[] a = {QNAN, 0.0, -0.0, -1.0, Double.NEGATIVE_INFINITY, 1.0}; Arrays.sort(a); return a; }
            case 72: { float[] a = {QNANF, 0f, -0f, -1f, Float.POSITIVE_INFINITY}; Arrays.sort(a); return a; }
            case 73: { int[] a = {9, 8, 7, 6, 5}; Arrays.sort(a, 1, 4); return a; }
            case 74: { int[] a = {9, 8, 7}; Arrays.sort(a, bad ? 2 : 0, 1); return a; }
            case 75: return Arrays.equals(new Object[] {null, "a"}, new Object[] {null, new String("a")});
            case 76: { byte[] a = seqB(1000); byte[] b = seqB(1000); b[999] = 0; return Arrays.mismatch(a, b); }
            case 77: { int[] a = seqI(1000); int[] b = seqI(1000); b[999] = 0; return Arrays.equals(a, b); }
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- Object / Thread / Class / Objects
    static final Object HOLD = new Object();
    static final int HOLD_H = System.identityHashCode(HOLD);
    static final String LIT = "identity-literal";
    static final int LIT_IH = System.identityHashCode(LIT);
    static final Thread MAIN = Thread.currentThread();
    static final Runnable LAMBDA = () -> { };
    static Object sinkObj;

    private static final class Priv { }

    static final String[] OB_ROWS = {"identity-stable", "identity-null", "object-hash-eq-identity",
        "identity-literal-stable", "currentThread-name", "currentThread-same", "isInstance-string",
        "isInstance-null", "isInstance-int-class", "isInstance-object-array", "isInstance-object-int-array",
        "isInstance-cloneable", "isInstance-serializable-2d", "isInstance-number-array", "isInstance-integer-array",
        "isInstance-lambda", "isArray", "getModifiers", "class-cast", "checkcast-message", "requireNonNull",
        "requireNonNull-message", "requireNonNull-supplier", "requireNonNull-null-supplier", "requireNonNullElse",
        "checkIndex", "checkIndex-negative", "checkFromToIndex", "checkFromIndexSize", "checkIndex-long",
        "objects-misc", "box-caches", "helpful-npe-invoke", "helpful-npe-aload", "helpful-npe-arraylength",
        "helpful-npe-getfield", "string-hash-vs-identity"};

    static int fieldF;

    static Object ob(int s, boolean bad) {
        switch (s) {
            case 0: {
                if ((K % 25000) == 24999) System.gc();
                sinkObj = new byte[64];
                return System.identityHashCode(HOLD) == HOLD_H && HOLD.hashCode() == HOLD_H;
            }
            case 1: return System.identityHashCode(null);
            case 2: { Object o = new Object(); return o.hashCode() == System.identityHashCode(o); }
            case 3: return System.identityHashCode(LIT) == LIT_IH;
            case 4: return Thread.currentThread().getName();
            case 5: return Thread.currentThread() == MAIN;
            case 6: return String.class.isInstance("s");
            case 7: return String.class.isInstance(null);
            case 8: return int.class.isInstance(1);
            case 9: return Object[].class.isInstance(new String[0]);
            case 10: return Object.class.isInstance(new int[0]);
            case 11: return Cloneable.class.isInstance(new int[0]);
            case 12: return java.io.Serializable.class.isInstance(new int[0][]);
            case 13: return Number[].class.isInstance(new Integer[0]);
            case 14: return Integer[].class.isInstance(new Number[0]);
            case 15: return Runnable.class.isInstance(LAMBDA);
            case 16: return new boolean[] {int[].class.isArray(), int.class.isArray(), Object.class.isArray(),
                String[][].class.isArray()};
            case 17: return new int[] {int[].class.getModifiers(), String[].class.getModifiers(),
                Object.class.getModifiers(), Priv[].class.getModifiers(), Runnable.class.getModifiers(),
                int.class.getModifiers(), Map.Entry.class.getModifiers(), Priv.class.getModifiers(),
                Priv[][].class.getModifiers(), Runnable[].class.getModifiers()};
            case 18: return String.class.cast(bad ? (Object) Integer.valueOf(7) : "s");
            case 19: { Object o = bad ? (Object) Integer.valueOf(7) : "s"; return (String) o; }
            case 20: return Objects.requireNonNull(bad ? null : "x");
            case 21: return Objects.requireNonNull(bad ? null : "x", "msg");
            case 22: return Objects.requireNonNull(bad ? null : "x", () -> "supplied");
            case 23: return Objects.requireNonNull(bad ? null : "x", (Supplier<String>) null);
            case 24: return Objects.requireNonNullElse(null, bad ? null : "d");
            case 25: return Objects.checkIndex(bad ? 5 : 2, 3);
            case 26: return Objects.checkIndex(bad ? -1 : 0, 3);
            case 27: return Objects.checkFromToIndex(bad ? 2 : 1, 1, 3);
            case 28: return Objects.checkFromIndexSize(1, bad ? 5 : 2, 3);
            case 29: return Objects.checkIndex(bad ? 5L : 2L, 3L);
            case 30: return new int[] {Objects.hash(1, 2, 3), Objects.hashCode(null), Objects.equals(null, null) ? 1 : 0,
                Objects.toString(null, "d").length(), Objects.hash()};
            case 31: return new boolean[] {Integer.valueOf(127) == Integer.valueOf(127),
                Integer.valueOf(128) == Integer.valueOf(128), Integer.valueOf(-128) == Integer.valueOf(-128),
                Long.valueOf(127) == Long.valueOf(127), Long.valueOf(128) == Long.valueOf(128),
                Character.valueOf((char) 127) == Character.valueOf((char) 127),
                Character.valueOf((char) 128) == Character.valueOf((char) 128),
                Short.valueOf((short) -128) == Short.valueOf((short) -128),
                Byte.valueOf((byte) -1) == Byte.valueOf((byte) -1), Boolean.valueOf(true) == Boolean.TRUE};
            case 32: { String s0 = bad ? null : "x"; return s0.length(); }
            case 33: { int[] a = bad ? null : new int[1]; return a[0]; }
            case 34: { int[] a = bad ? null : new int[1]; return a.length; }
            case 35: { Holder h = bad ? null : HOLDER; return h.f; }
            case 36: { String s0 = new String(LIT); return s0.hashCode() == LIT.hashCode() && System.identityHashCode(s0) != 0; }
            default: throw new IllegalStateException();
        }
    }

    static final class Holder { int f = 3; }
    static final Holder HOLDER = new Holder();

    // ---------------------------------------------------------------- String scenarios
    static final String L1 = "hello world";
    static final String L2 = "hello w";
    static final String U1 = "hello Ā world";
    static final String U2 = "Ā";
    static final String SUPP = "a😀b";
    static final String LAT = "café";
    static final String L1C = new String(L1.toCharArray());
    static final String U1C = new String(U1.toCharArray());
    static final String LATC = new String(LAT.toCharArray());
    static final String EMPTYC = new String(new char[0]);
    static final StringBuilder SBOBJ = new StringBuilder(L1);
    static final int[] NEEDLES = {-1, 0x110000, 0x1F600, 0xD83D, 0xDE00, 'o', 0x100, 0xE9, 0x1006F, 0, 'b', 0xFFFF};

    static final String[] ST_ROWS = {"equals-latin1-copy", "equals-latin1-utf16", "equals-utf16-copy",
        "equals-empty", "equals-null", "equals-non-string", "equals-prefix", "equals-latin1-high",
        "hashCode-latin1-fresh", "hashCode-utf16-fresh", "hashCode-min-value", "hashCode-zero-twice",
        "hashCode-empty", "hashCode-supplementary", "hashCode-latin1-high", "indexOf-char-constants",
        "indexOf-char-variable", "indexOf-string", "indexOf-null", "compareTo", "compareTo-null", "charAt-past-end",
        "charAt-negative-utf16", "substring-begin-gt-end", "length-isEmpty", "indexOf-char-range-bad",
        "lastIndexOf-variable"};

    static Object st(int s, boolean bad) {
        switch (s) {
            case 0: return L1.equals(L1C);
            case 1: return L1.equals(U1);
            case 2: return U1.equals(U1C);
            case 3: return "".equals(EMPTYC);
            case 4: return L1.equals(null);
            case 5: return L1.equals(SBOBJ);
            case 6: return L1.equals(L2);
            case 7: return LAT.equals(LATC);
            case 8: return new String(L1.toCharArray()).hashCode();
            case 9: return new String(U1.toCharArray()).hashCode();
            case 10: return new String("polygenelubricants".toCharArray()).hashCode();
            case 11: { String z = new String("f5a5a608".toCharArray()); return z.hashCode() * 31 + z.hashCode(); }
            case 12: return EMPTYC.hashCode();
            case 13: return new String(SUPP.toCharArray()).hashCode();
            case 14: return new String(LAT.toCharArray()).hashCode();
            case 15: return new int[] {L1.indexOf('o'), U1.indexOf(0x100), U1.indexOf('w'), SUPP.indexOf(0x1F600),
                SUPP.indexOf(0xD83D), SUPP.indexOf(0xDE00), L1.indexOf(-1), L1.indexOf(0x110000), L1.indexOf(0x1006F),
                L1.indexOf('o', 5), L1.indexOf('o', -5), L1.indexOf('o', 100), L1.lastIndexOf('o'), LAT.indexOf(0xE9),
                LAT.indexOf(0x1E9), U1.lastIndexOf(0x100), SUPP.lastIndexOf(0x1F600), L1.indexOf('o', 2, 6),
                U1.indexOf(0xFF), L1.indexOf('d'), "".indexOf('a')};
            case 16: {
                int[] r = new int[NEEDLES.length * 4];
                for (int k = 0; k < NEEDLES.length; k++) {
                    int n = NEEDLES[k];
                    r[k * 4] = L1.indexOf(n);
                    r[k * 4 + 1] = U1.indexOf(n);
                    r[k * 4 + 2] = SUPP.indexOf(n);
                    r[k * 4 + 3] = LAT.indexOf(n);
                }
                return r;
            }
            case 17: return new int[] {L1.indexOf("world"), L1.indexOf(""), "".indexOf(""), L1.indexOf(U2),
                U1.indexOf("world"), U1.indexOf("Ā w"), L1.indexOf("o", 5), L1.lastIndexOf("o"), U1.indexOf(U2),
                SUPP.indexOf("\uDE00"), L1.indexOf("world", -3), L1.indexOf("", 100), L1.lastIndexOf("", 100),
                L1.contains("lo w") ? 1 : 0, L1.indexOf("hello world!"), U1.indexOf("hello"), LAT.indexOf("é")};
            case 18: return L1.indexOf(bad ? null : "o");
            case 19: return new int[] {L1.compareTo(L2), L1.compareTo(U1), U1.compareTo(L1), "a".compareTo("é"),
                "é".compareTo("Ā"), "".compareTo("a"), L1.compareTo(L1C), U1.compareTo(U1C),
                SUPP.compareTo(U1), "￿".compareTo("\uD800"), LAT.compareTo(U1)};
            case 20: return L1.compareTo(bad ? null : "x");
            case 21: return (int) L1.charAt(bad ? 11 : 10);
            case 22: return (int) U1.charAt(bad ? -1 : 6);
            case 23: return L1.substring(bad ? 2 : 1, 1);
            case 24: return new int[] {L1.length(), U1.length(), "".length(), SUPP.length(), L1.isEmpty() ? 1 : 0,
                EMPTYC.isEmpty() ? 1 : 0, U2.length()};
            case 25: return L1.indexOf('o', 5, bad ? 3 : 8);
            case 26: {
                int[] r = new int[NEEDLES.length * 2];
                for (int k = 0; k < NEEDLES.length; k++) {
                    r[k * 2] = U1.lastIndexOf(NEEDLES[k]);
                    r[k * 2 + 1] = SUPP.lastIndexOf(NEEDLES[k]);
                }
                return r;
            }
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- StringBuilder
    static final int[] SB_INTS = {MIN, -1, 0, 9, 10, 99, 100, MAX, -1000000000, 1000000000, -2147483647, 7, -10};
    static final long[] SB_LONGS = {LMIN, LMAX, -1L, 0L, 10_000_000_000L, -9_999_999_999L, 1L << 53, MIN, MAX};
    static final char[] SB_CHARS = {'a', 'é', 'Ā', '\uD83D', '\uDE00', '\u0000', '￿'};
    static final float[] SB_FLOATS = {1.0E10f, 0.1f, Float.NaN, -0f, 1.4E-45f, 3.4028235E38f, 1.0E-5f, 100f};
    static final double[] SB_DOUBLES = {2e23, Double.MIN_VALUE, -0.0, 1e-5, 0.001, 1e7, 9007199254740993.0,
        Double.NaN, Double.NEGATIVE_INFINITY};
    static final String[] SB_ROWS = {"append-int", "append-long", "append-char", "append-float", "append-double",
        "append-misc", "insert-delete-reverse", "charAt-past-end", "setLength-negative", "concat-indy",
        "append-int-latin1-then-utf16"};

    static Object sb(int s, boolean bad) {
        StringBuilder b = new StringBuilder();
        switch (s) {
            case 0: for (int v : SB_INTS) b.append(v).append(','); return b.toString();
            case 1: for (long v : SB_LONGS) b.append(v).append(','); return b.toString();
            case 2: for (char v : SB_CHARS) b.append(v); return b.toString() + "/" + b.length();
            case 3: for (float v : SB_FLOATS) b.append(v).append(','); return b.toString();
            case 4: for (double v : SB_DOUBLES) b.append(v).append(','); return b.toString();
            case 5: b.append(true).append(false).append((Object) null).appendCodePoint(0x1F600);
                b.appendCodePoint(bad ? -1 : 'x');
                return b.toString();
            case 6: b.append("abcdef").insert(0, MIN).insert(3, "xyz").deleteCharAt(2);
                b.append("a😀b").reverse();
                return b.toString();
            case 7: b.append("abc"); return (int) b.charAt(bad ? 5 : 2);
            case 8: b.append("abc"); b.setLength(bad ? -1 : 1); return b.toString();
            case 9: return "" + MIN + "," + LMIN + "," + 'c' + "," + 1.0E10f + "," + 2e23 + "," + true + "," + (Object) null
                + "," + (char) 0x100 + MAX;
            case 10: b.append(123).append('Ā').append(-45).append(LMIN); return b.toString();
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- number toString
    static final String[] NU_ROWS = {"Integer.toString", "Integer.toString-radix", "Integer.hex-oct-bin-unsigned",
        "Long.toString-family", "valueOf-family"};
    static final int[] NU_RADIX = {2, 8, 16, 36, 37, 1, -5, 10, 0, 7};

    static Object nu(int s, boolean bad) {
        switch (s) {
            case 0: return new String[] {Integer.toString(MIN), Integer.toString(MAX), Integer.toString(-1),
                Integer.toString(0), Integer.toString(10), Integer.toString(-10), Integer.toString(1_000_000_007)};
            case 1: {
                String[] r = new String[NU_RADIX.length * 2];
                for (int k = 0; k < NU_RADIX.length; k++) {
                    r[k * 2] = Integer.toString(MIN, NU_RADIX[k]);
                    r[k * 2 + 1] = Integer.toString(-255, NU_RADIX[k]);
                }
                return r;
            }
            case 2: return new String[] {Integer.toHexString(MIN), Integer.toOctalString(-1), Integer.toBinaryString(5),
                Integer.toUnsignedString(-1), Integer.toUnsignedString(-1, 36), Integer.toUnsignedString(-1, 99),
                Integer.toHexString(0), Integer.toBinaryString(MIN)};
            case 3: return new String[] {Long.toString(LMIN), Long.toString(LMAX), Long.toString(LMIN, 36),
                Long.toString(-255, 16), Long.toHexString(-1), Long.toUnsignedString(-1), Long.toBinaryString(LMIN),
                Long.toOctalString(LMIN), Long.toUnsignedString(-1, 36), Long.toString(LMIN, 37)};
            case 4: return new String[] {String.valueOf(MIN), String.valueOf(LMIN), String.valueOf('Ā'),
                Integer.valueOf(MIN).toString(), Long.valueOf(LMIN).toString(), Short.toString((short) -32768),
                Byte.toString((byte) -128), Character.toString(0x1F600), Boolean.toString(false)};
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- checksums
    static final byte[] DATA = new byte[4096];
    static {
        for (int i = 0; i < DATA.length; i++) DATA[i] = (byte) ((i * 31 + 7) ^ (i >>> 3));
    }
    static final ByteBuffer DIRECT = ByteBuffer.allocateDirect(4096);
    static {
        DIRECT.put(DATA);
        DIRECT.clear();
    }
    static final String[] CR_ROWS = {"crc32-array", "crc32-int-above-255", "crc32-heap-buffer", "crc32-direct-buffer",
        "crc32c-array", "crc32c-int", "crc32c-direct-buffer", "adler32-array", "adler32-int", "adler32-direct-buffer",
        "crc32-null", "crc32-negative-off", "crc32-past-end", "crc32c-past-end", "adler32-past-end",
        "crc32-reset-partial", "checksum-default-update"};

    static long crcRange(Checksum c) {
        int off = (K * 37) & 1023;
        int len = (K * 101) % 3000;
        c.update(DATA, off, len);
        return c.getValue();
    }

    static long crcInts(Checksum c) {
        for (int t = 0; t < 64; t++) c.update(K * 7 + t * 13);
        return c.getValue();
    }

    static long crcDirect(Checksum c) {
        ByteBuffer d = DIRECT.duplicate();
        d.position((K * 13) & 511).limit(1024 + ((K * 7) & 2047));
        if (c instanceof CRC32) ((CRC32) c).update(d);
        else if (c instanceof CRC32C) ((CRC32C) c).update(d);
        else ((Adler32) c).update(d);
        return c.getValue() * 31 + d.position();
    }

    static Object cr(int s, boolean bad) {
        switch (s) {
            case 0: return crcRange(new CRC32());
            case 1: return crcInts(new CRC32());
            case 2: {
                ByteBuffer h = ByteBuffer.wrap(DATA, (K * 5) & 255, 1500).slice();
                CRC32 c = new CRC32();
                c.update(h);
                return c.getValue();
            }
            case 3: return crcDirect(new CRC32());
            case 4: return crcRange(new CRC32C());
            case 5: return crcInts(new CRC32C());
            case 6: return crcDirect(new CRC32C());
            case 7: return crcRange(new Adler32());
            case 8: return crcInts(new Adler32());
            case 9: return crcDirect(new Adler32());
            case 10: { CRC32 c = new CRC32(); c.update(bad ? null : DATA, 0, 1); return c.getValue(); }
            case 11: { CRC32 c = new CRC32(); c.update(DATA, bad ? -1 : 0, 2); return c.getValue(); }
            case 12: { CRC32 c = new CRC32(); c.update(DATA, 4000, bad ? 200 : 96); return c.getValue(); }
            case 13: { CRC32C c = new CRC32C(); c.update(DATA, 4000, bad ? 200 : 96); return c.getValue(); }
            case 14: { Adler32 c = new Adler32(); c.update(DATA, 4000, bad ? 200 : 96); return c.getValue(); }
            case 15: { CRC32 c = new CRC32(); c.update(DATA); c.reset(); c.update(DATA, 10, 20); return c.getValue(); }
            case 16: { Checksum c = (K & 1) == 0 ? new CRC32() : new CRC32C(); c.update(DATA); return c.getValue(); }
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- Base64
    static final String[] B6_ROWS = {"basic-encode", "basic-roundtrip", "url-encode", "mime-encode",
        "without-padding", "decode-illegal-char", "decode-one-char", "decode-bad-padding", "url-decode-plus",
        "mime-decode-junk", "encode-bytebuffer", "basic-decode-space", "mime-custom-wrap", "mime-bad-separator"};

    static Object b6(int s, boolean bad) {
        int len = K % 70;
        byte[] in = Arrays.copyOfRange(DATA, K & 255, (K & 255) + len);
        switch (s) {
            case 0: return Base64.getEncoder().encodeToString(in);
            case 1: return Arrays.equals(Base64.getDecoder().decode(Base64.getEncoder().encode(in)), in);
            case 2: return Base64.getUrlEncoder().encodeToString(in);
            case 3: return Base64.getMimeEncoder().encodeToString(Arrays.copyOf(DATA, 200 + len));
            case 4: return Base64.getEncoder().withoutPadding().encodeToString(in);
            case 5: return Base64.getDecoder().decode(bad ? "ab*d" : "abcd");
            case 6: return Base64.getDecoder().decode(bad ? "a" : "ab");
            case 7: return Base64.getDecoder().decode(bad ? "ab=c" : "ab==");
            case 8: return Base64.getUrlDecoder().decode(bad ? "ab+d" : "ab-d");
            case 9: return Base64.getMimeDecoder().decode("YW*Jj\r\nZA==");
            case 10: { ByteBuffer o = Base64.getEncoder().encode(ByteBuffer.wrap(in)); return o.remaining() * 1000 + o.get(0); }
            case 11: return Base64.getDecoder().decode(bad ? "ab d" : "abcd");
            case 12: return Base64.getMimeEncoder(4, new byte[] {'\r', '\n'}).encodeToString(Arrays.copyOf(DATA, 10));
            case 13: return Base64.getMimeEncoder(4, bad ? new byte[] {'a'} : new byte[] {'\n'}).encodeToString(in);
            default: throw new IllegalStateException();
        }
    }

    // ---------------------------------------------------------------- toString sweeps
    static String dtoaSweep(int kind) {
        long h = 0;
        int bad = 0;
        long seed = 0x9E3779B97F4A7C15L;
        for (int i = 0; i < ITER; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            double d;
            if (kind == 0) d = bd(seed);                                  // any bit pattern
            else if (kind == 1) d = bd((seed >>> 12) & 0x000fffffffffffffL);  // subnormals
            else d = (i - ITER / 2) / 1000.0;                             // decimals
            String s = Double.toString(d);
            h = h * 31 + s.hashCode();
            if (!Double.isNaN(d) && raw(Double.parseDouble(s)) != raw(d)) bad++;
        }
        return "roundtrip-fail=" + bad + " h=" + Long.toHexString(h);
    }

    static String ftoaSweep(int kind) {
        long h = 0;
        int bad = 0;
        long seed = 0x2545F4914F6CDD1DL;
        for (int i = 0; i < ITER; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            float f;
            if (kind == 0) f = bf((int) (seed >>> 32));
            else if (kind == 1) f = bf((int) ((seed >>> 41) & 0x007fffff));
            else f = bf(0x3f800000 + i);                                  // from 1.0f upward
            String s = Float.toString(f);
            h = h * 31 + s.hashCode();
            if (!Float.isNaN(f) && Float.floatToRawIntBits(Float.parseFloat(s)) != Float.floatToRawIntBits(f)) bad++;
        }
        return "roundtrip-fail=" + bad + " h=" + Long.toHexString(h);
    }

    // ---------------------------------------------------------------- timing
    static long tsink;

    interface Bench { long run(int n); }

    static void time(String name, int n, Bench b) {
        for (int w = 0; w < 5; w++) tsink += b.run(n);
        double[] reps = new double[7];
        for (int r = 0; r < reps.length; r++) {
            long t0 = System.nanoTime();
            tsink += b.run(n);
            reps[r] = (System.nanoTime() - t0) / (double) n;
        }
        Arrays.sort(reps);
        System.out.println("time " + name + " " + String.format(java.util.Locale.ROOT, "%.1f", reps[reps.length / 2]) + " ns/op");
    }

    static final int[] TA = seqI(1024);
    static final int[] TB = mutateLast(seqI(1024));
    static final byte[] TBA = seqB(4096);
    static final byte[] TBB = seqB(4096);
    static final String TL = "a".repeat(999) + "z";
    static final String TU = "Ā".repeat(999) + "z";

    static int[] mutateLast(int[] a) { a[a.length - 1] = -7; return a; }

    static void timings() {
        time("arrays-mismatch-int-1024", 20_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Arrays.mismatch(TA, TB); return s; });
        time("arrays-equals-byte-4096", 20_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Arrays.equals(TBA, TBB) ? 1 : 0; return s; });
        time("arrays-hashcode-int-1024", 20_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Arrays.hashCode(TA); return s; });
        time("string-indexof-char-latin1-1000", 50_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += TL.indexOf('z'); return s; });
        time("string-indexof-char-utf16-1000", 50_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += TU.indexOf('z'); return s; });
        time("string-indexof-var-char-latin1-1000", 50_000, n -> { long s = 0; char c = (char) ('y' + (n & 1) * 0 + 1); for (int i = 0; i < n; i++) s += TL.indexOf(c); return s; });
        time("integer-tostring", 500_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Integer.toString(i * 2654435761L > 0 ? i * 7919 : -i).length(); return s; });
        time("long-tostring", 500_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Long.toString(i * 0x9E3779B97F4A7C15L).length(); return s; });
        time("sb-append-int", 500_000, n -> { StringBuilder b = new StringBuilder(); long s = 0; for (int i = 0; i < n; i++) { b.setLength(0); b.append(i * 7919 - 1_000_000); s += b.length(); } return s; });
        time("double-tostring", 200_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Double.toString(i * 0.37 + 1e-3).length(); return s; });
        time("float-tostring", 200_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Float.toString(i * 0.37f + 1e-3f).length(); return s; });
        time("crc32-update-4096", 5_000, n -> { long s = 0; for (int i = 0; i < n; i++) { CRC32 c = new CRC32(); c.update(TBA, 0, 4096); s += c.getValue(); } return s; });
        time("crc32c-update-4096", 5_000, n -> { long s = 0; for (int i = 0; i < n; i++) { CRC32C c = new CRC32C(); c.update(TBA, 0, 4096); s += c.getValue(); } return s; });
        time("adler32-update-4096", 5_000, n -> { long s = 0; for (int i = 0; i < n; i++) { Adler32 c = new Adler32(); c.update(TBA, 0, 4096); s += c.getValue(); } return s; });
        time("base64-encode-1024", 20_000, n -> { long s = 0; byte[] in = Arrays.copyOf(TBA, 1024); for (int i = 0; i < n; i++) s += Base64.getEncoder().encode(in).length; return s; });
        time("math-floordiv-int", 2_000_000, n -> { long s = 0; for (int i = 1; i <= n; i++) s += Math.floorDiv(i * 31, -((i & 7) + 1)); return s; });
        time("math-addexact-int", 2_000_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Math.addExact(i, i >> 1); return s; });
        time("math-round-double", 2_000_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Math.round(i * 0.75); return s; });
        time("integer-parseint", 500_000, n -> { long s = 0; String[] t = {"123", "-45678", "2147483647", "0"}; for (int i = 0; i < n; i++) s += Integer.parseInt(t[i & 3]); return s; });
        time("character-isdigit-supplementary", 2_000_000, n -> { long s = 0; for (int i = 0; i < n; i++) s += Character.isDigit(0x1D7CE + (i & 15)) ? 1 : 0; return s; });
        time("system-arraycopy-int-64", 1_000_000, n -> { long s = 0; int[] d = new int[64]; for (int i = 0; i < n; i++) { System.arraycopy(TA, i & 511, d, 0, 64); s += d[i & 63]; } return s; });
    }

    // ---------------------------------------------------------------- main
    static void numericRows(String prefix, int fam, String[] ops, int n) {
        for (int op = 0; op < ops.length; op++) {
            final int o = op;
            row(prefix + "." + ops[op], () -> runList(fam, o, n, true));
        }
    }

    static void scenarioRows(String prefix, int fam, String[] rows) {
        for (int s = 0; s < rows.length; s++) {
            final int sc = s;
            row(prefix + "." + rows[s], () -> runList(fam, sc, 1, false));
        }
    }

    public static void main(String[] args) {
        boolean timing = true;
        for (String a : args) {
            if (a.equals("notime")) timing = false;
            else ITER = Integer.parseInt(a);
        }
        System.out.println("R21IntrinsicCorners iter=" + ITER + " passes=" + PASSES);
        numericRows("math.d1", F_D1, D1_OPS, D_M.length);
        numericRows("math.ds", F_DS, DS_OPS, D_S.length);
        numericRows("math.d2", F_D2, D2_OPS, D2A.length);
        numericRows("math.fma", F_D3, D3_OPS, FMA_A.length);
        numericRows("math.f1", F_F1, F1_OPS, F_M.length);
        numericRows("math.fs", F_FS, FS_OPS, F_S.length);
        numericRows("math.f2", F_F2, F2_OPS, F2A.length);
        numericRows("math.i2", F_I2, I2_OPS, I2A.length);
        numericRows("math.l2", F_L2, L2_OPS, L2A.length);
        numericRows("bits.int", F_IB, IB_OPS, I_BITS.length);
        numericRows("bits.long", F_LB, LB_OPS, L_BITS.length);
        numericRows("char", F_CH, CH_OPS, CPS.length);
        for (int op = 0; op < DT_OPS.length; op++) {
            final int o = op;
            int n = op < 6 ? D_TOSTR.length : F_TOSTR.length;
            row("dt." + DT_OPS[op], () -> runList(F_DT, o, n, true));
        }
        for (int op = 0; op < PA_OPS.length; op++) {
            final int o = op;
            row("pa." + PA_OPS[op], () -> runList(F_PA, o, P_STR.length, false));
        }
        scenarioRows("ac", S_AC, AC_ROWS);
        scenarioRows("ar", S_AR, AR_ROWS);
        scenarioRows("ob", S_OB, OB_ROWS);
        scenarioRows("st", S_ST, ST_ROWS);
        scenarioRows("sb", S_SB, SB_ROWS);
        scenarioRows("nu", S_NU, NU_ROWS);
        scenarioRows("cr", S_CR, CR_ROWS);
        scenarioRows("b6", S_B6, B6_ROWS);
        row("dt.sweep-double-bits", () -> dtoaSweep(0));
        row("dt.sweep-double-subnormal", () -> dtoaSweep(1));
        row("dt.sweep-double-decimal", () -> dtoaSweep(2));
        row("dt.sweep-float-bits", () -> ftoaSweep(0));
        row("dt.sweep-float-subnormal", () -> ftoaSweep(1));
        row("dt.sweep-float-from-one", () -> ftoaSweep(2));
        // Thread.currentThread on a second thread, hot.
        final String[] other = new String[1];
        Thread t = new Thread(() -> {
            Thread me = Thread.currentThread();
            int same = 0;
            for (int i = 0; i < ITER; i++) if (Thread.currentThread() == me) same++;
            other[0] = me.getName() + " same=" + (same == ITER) + " notMain=" + (me != MAIN);
        }, "probe-other");
        t.start();
        try {
            t.join();
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
        System.out.println("ob.currentThread-other " + other[0]);
        if (timing) timings();
        System.out.println("done tsink=" + (tsink != 0));
    }
}
