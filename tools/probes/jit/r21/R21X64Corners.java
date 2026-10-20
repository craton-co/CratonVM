// JIT round 21 wave 3, lane x64emit: arithmetic, compare, conversion and narrowing corners of the
// x86-64 instruction selection, in hot compiled loops.
//
// Every row is a static method that folds the results of one operation family over a table of
// corner inputs (int/long div/rem by -1 and MIN_VALUE, shifts by large and negative counts,
// d2i/d2l/f2i/f2l of NaN/inf/huge, Math.min/max with -0.0 and NaN, char/short/byte widening,
// unsigned compares/divides, multiplyHigh, the Integer/Long bit intrinsics, frem/drem, floor/
// ceil/rint/round, the raw FP-bits reinterpretations) into one 64-bit hash. The inputs are read
// out of arrays so neither javac nor a JIT can fold them. Each row is run once cold (first call,
// interpreted) and then WARM times (single-pass, then the optimizing tier, OSR in between); the
// probe prints the hot hash and whether it equals the cold one.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21X64Corners.java):
//   P=tools/probes/out
//   cratonvm -Xmx256m -cp $P R21X64Corners                       # default tiers
//   cratonvm --compatible -Xmx256m -cp $P R21X64Corners          # both modes, same lines
//   CRATONVM_JIT_IR=0 cratonvm -Xmx256m -cp $P R21X64Corners     # single-pass only (if the
//                                                                #   build still honours it)
//   cratonvm --nojit -Xmx256m -cp $P R21X64Corners               # interpreter control
//   java -Xmx256m -cp $P R21X64Corners                           # HotSpot 25
// Kill-switch control for this lane's fix (single-pass `Float.floatToRawIntBits` sign-extension):
//   CRATONVM_JIT_X64_RAW_INT_BITS_SEXT=0 cratonvm -Xmx256m -cp $P R21X64Corners
// may change the `row rawbits-widen` hash and print `stable false` for it (the defect: a negative
// float's bits widened through a spliced `(long) int` read as 2^32 + bits); every other line is
// unchanged.
//
// HotSpot expectation: every line identical to HotSpot 25's (diff them); every row says
// `stable true`. Lines are `row <name> <hash-hex> stable <bool>`, then a few `fact` lines
// spelling out single values for readability. No timing lines. A NaN an operation COMPUTES
// (inf % 1, sqrt(-1), inf * 0) is hashed canonicalised (`doubleToLongBits`): its raw bits are
// the platform's default NaN and not a Java-visible contract. A NaN an operation SELECTS
// (min/max/abs/neg of an argument) is hashed raw: its bits are the argument's.

public class R21X64Corners {
    static final int WARM = 3000;

    static final int[] INTS = {
        0, 1, -1, 2, -2, 3, -3, 7, -7, 8, -8, 10, 31, 32, 33, 63, 64, 65, -31, -32, -33, -63, -64,
        100, -100, 127, -128, 255, 32767, -32768, 65535, 65536, 1 << 30, (1 << 30) + 1,
        0x12345678, 0xDEADBEEF, 0x7FFFFFFF, 0x80000000, 0x80000001, 0xFFFF0000, 0x00008000,
    };

    static final long[] LONGS = {
        0L, 1L, -1L, 2L, -2L, 3L, -3L, 7L, -7L, 10L, -10L, 63L, 64L, 65L, -64L, -65L,
        0x7FFFFFFFL, 0x80000000L, -0x80000000L, -0x80000001L, 0xFFFFFFFFL, 1L << 32, (1L << 32) + 1,
        1L << 40, 1L << 62, (1L << 62) + 1, 0x123456789ABCDEF0L, 0xFEDCBA9876543210L,
        Long.MAX_VALUE, Long.MIN_VALUE, Long.MIN_VALUE + 1, Long.MAX_VALUE - 1,
    };

    static final int[] SHIFTS = {
        0, 1, 5, 15, 16, 31, 32, 33, 47, 63, 64, 65, 95, -1, -5, -31, -32, -33, -63, -64,
        Integer.MAX_VALUE, Integer.MIN_VALUE,
    };

    static final double[] DOUBLES = {
        Double.NaN, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, 0.0, -0.0, 0.5, -0.5,
        1.5, -1.5, 2.5, -2.5, 0.49999999999999994, -0.49999999999999994, 1e-300, -1e-300,
        Double.MIN_VALUE, -Double.MIN_VALUE, Double.MAX_VALUE, -Double.MAX_VALUE,
        2147483647.0, 2147483647.9, 2147483648.0, -2147483648.0, -2147483648.9, -2147483649.0,
        4294967296.5, 9.2e18, 9.223372036854775807e18, 9.3e18, -9.223372036854775808e18, -9.3e18,
        12345.678, -98765.4321, 3.0, 7.0, -7.0,
    };

    static final float[] FLOATS = {
        Float.NaN, Float.POSITIVE_INFINITY, Float.NEGATIVE_INFINITY, 0.0f, -0.0f, 0.5f, -0.5f,
        1.5f, -1.5f, 2.5f, -2.5f, 0.49999997f, -0.49999997f, Float.MIN_VALUE, -Float.MIN_VALUE,
        Float.MAX_VALUE, -Float.MAX_VALUE, 2147483647.0f, 2147483648.0f, -2147483648.0f,
        -2147483904.0f, 9.2e18f, 9.3e18f, -9.3e18f, 12345.678f, -98765.43f, 3.0f, -7.0f,
    };

    // FP min/max operands: ONE NaN of each width (two distinct NaNs would test which NaN a
    // min/max answers, where HotSpot's own intrinsic and the JDK body disagree), plus a payload
    // NaN used only against non-NaN operands (its bits must come back exactly).
    static final double[] MM_D = {
        Double.NaN, -0.0, 0.0, 1.0, -1.0, 3.5, -3.5, Double.POSITIVE_INFINITY,
        Double.NEGATIVE_INFINITY, Double.MIN_VALUE, -Double.MIN_VALUE,
    };
    static final float[] MM_F = {
        Float.NaN, -0.0f, 0.0f, 1.0f, -1.0f, 3.5f, -3.5f, Float.POSITIVE_INFINITY,
        Float.NEGATIVE_INFINITY, Float.MIN_VALUE, -Float.MIN_VALUE,
    };
    static final double PAYLOAD_NAN_D = Double.longBitsToDouble(0x7FF8000000ABCDEFL);
    static final float PAYLOAD_NAN_F = Float.intBitsToFloat(0x7FC0ABCD);

    static final short[] SHORTS = {0, 1, -1, 127, -128, 255, 32767, -32768, -2, 0x1234};
    static final char[] CHARS = {0, 1, 0x7F, 0x80, 0xFF, 0x7FFF, 0x8000, 0xFFFF, 0xFFFE, 0x1234};
    static final byte[] BYTES = {0, 1, -1, 127, -128, 0x55, (byte) 0xAA, -2};

    static long mix(long h, long v) {
        return (h ^ v) * 0x100000001B3L + (v >>> 29);
    }

    // ── int / long division ────────────────────────────────────────────────────────────────

    static long divInt(int[] a) {
        long h = 17;
        for (int i = 0; i < a.length; i++) {
            for (int j = 0; j < a.length; j++) {
                int x = a[i], y = a[j];
                if (y == 0) continue;
                h = mix(h, x / y);
                h = mix(h, x % y);
            }
        }
        return h;
    }

    static long divLong(long[] a) {
        long h = 19;
        for (int i = 0; i < a.length; i++) {
            for (int j = 0; j < a.length; j++) {
                long x = a[i], y = a[j];
                if (y == 0) continue;
                h = mix(h, x / y);
                h = mix(h, x % y);
            }
        }
        return h;
    }

    static long divConstInt(int[] a) {
        long h = 23;
        for (int i = 0; i < a.length; i++) {
            int x = a[i];
            h = mix(h, x / -1);
            h = mix(h, x % -1);
            h = mix(h, x / Integer.MIN_VALUE);
            h = mix(h, x % Integer.MIN_VALUE);
            h = mix(h, x / Integer.MAX_VALUE);
            h = mix(h, x % Integer.MAX_VALUE);
            h = mix(h, x / 3);
            h = mix(h, x % 3);
            h = mix(h, x / -3);
            h = mix(h, x % -3);
            h = mix(h, x / 8);
            h = mix(h, x % 8);
            h = mix(h, x / -8);
            h = mix(h, x % -8);
            h = mix(h, x / 7);
            h = mix(h, x % 7);
            h = mix(h, x / 10);
            h = mix(h, x % 10);
            h = mix(h, x / 1000000007);
            h = mix(h, x % 1000000007);
            h = mix(h, x / 1073741825); // 2^30 + 1, the magic-number corner
            h = mix(h, x / (1 << 30));
            h = mix(h, x % (1 << 30));
            h = mix(h, x / 1);
            h = mix(h, x % 1);
        }
        return h;
    }

    static long divConstLong(long[] a) {
        long h = 29;
        for (int i = 0; i < a.length; i++) {
            long x = a[i];
            h = mix(h, x / -1L);
            h = mix(h, x % -1L);
            h = mix(h, x / Long.MIN_VALUE);
            h = mix(h, x % Long.MIN_VALUE);
            h = mix(h, x / 10L);
            h = mix(h, x % 10L);
            h = mix(h, x / -16L);
            h = mix(h, x % -16L);
            h = mix(h, x / 7L);
            h = mix(h, x % 7L);
            h = mix(h, x / (1L << 40));
            h = mix(h, x % (1L << 40));
            h = mix(h, x / ((1L << 62) + 1));
            h = mix(h, x % ((1L << 62) + 1));
            h = mix(h, x / 1000000007L);
            h = mix(h, x % 1000000007L);
            h = mix(h, x / 0x7FFFFFFFFFFFL);
        }
        return h;
    }

    static long divZero(int[] a) {
        long h = 31;
        for (int i = 0; i < a.length; i++) {
            int zero = a[0]; // 0
            try {
                h = mix(h, a[i] / zero);
            } catch (ArithmeticException e) {
                h = mix(h, e.getMessage().length());
            }
            try {
                h = mix(h, (long) a[i] % (long) zero);
            } catch (ArithmeticException e) {
                h = mix(h, 7 + e.getMessage().length());
            }
        }
        return h;
    }

    // ── shifts ─────────────────────────────────────────────────────────────────────────────

    static long shiftVar(int[] a, long[] l, int[] s) {
        long h = 37;
        for (int i = 0; i < a.length; i++) {
            for (int j = 0; j < s.length; j++) {
                int x = a[i], c = s[j];
                h = mix(h, x << c);
                h = mix(h, x >> c);
                h = mix(h, x >>> c);
            }
        }
        for (int i = 0; i < l.length; i++) {
            for (int j = 0; j < s.length; j++) {
                long x = l[i];
                int c = s[j];
                h = mix(h, x << c);
                h = mix(h, x >> c);
                h = mix(h, x >>> c);
            }
        }
        return h;
    }

    static long shiftConst(int[] a, long[] l) {
        long h = 41;
        for (int i = 0; i < a.length; i++) {
            int x = a[i];
            h = mix(h, x << 33);
            h = mix(h, x >> -1);
            h = mix(h, x >>> 32);
            h = mix(h, x >>> 0);
            h = mix(h, x << -31);
            h = mix(h, x >>> 31);
            h = mix(h, x >> 31);
            h = mix(h, (long) (x >>> 1));
            h = mix(h, (long) (x << 16));
        }
        for (int i = 0; i < l.length; i++) {
            long x = l[i];
            h = mix(h, x << 65);
            h = mix(h, x >>> -1);
            h = mix(h, x >> 64);
            h = mix(h, x << 0);
            h = mix(h, x >>> 32);
            h = mix(h, x >> 63);
            h = mix(h, x << 40);
            h = mix(h, (int) (x >>> 33));
        }
        return h;
    }

    // ── conversions ────────────────────────────────────────────────────────────────────────

    static long convD(double[] d) {
        long h = 43;
        for (int i = 0; i < d.length; i++) {
            double x = d[i];
            h = mix(h, (int) x);
            h = mix(h, (long) x);
            h = mix(h, Float.floatToIntBits((float) x));
            h = mix(h, (short) (int) x);
            h = mix(h, (char) (int) x);
            h = mix(h, (byte) (int) x);
            h = mix(h, (int) (x * 2.0));
            h = mix(h, (long) (x + 0.5));
            h = mix(h, (int) (float) x);
        }
        return h;
    }

    static long convF(float[] f) {
        long h = 47;
        for (int i = 0; i < f.length; i++) {
            float x = f[i];
            h = mix(h, (int) x);
            h = mix(h, (long) x);
            h = mix(h, Double.doubleToLongBits((double) x));
            h = mix(h, (int) (x * 3.0f));
            h = mix(h, (long) (x - 0.25f));
        }
        return h;
    }

    static long convInt(int[] a, long[] l) {
        long h = 53;
        for (int i = 0; i < a.length; i++) {
            int x = a[i];
            h = mix(h, (byte) x);
            h = mix(h, (short) x);
            h = mix(h, (char) x);
            h = mix(h, (long) (char) x);
            h = mix(h, (long) (short) x);
            h = mix(h, Double.doubleToRawLongBits((double) x));
            h = mix(h, Float.floatToRawIntBits((float) x));
            h = mix(h, (long) x * 3L);
        }
        for (int i = 0; i < l.length; i++) {
            long x = l[i];
            h = mix(h, (int) x);
            h = mix(h, (long) (int) x);
            h = mix(h, Double.doubleToRawLongBits((double) x));
            h = mix(h, Float.floatToRawIntBits((float) x));
            h = mix(h, (short) x);
            h = mix(h, (char) x);
        }
        return h;
    }

    // ── narrow loads ───────────────────────────────────────────────────────────────────────

    static long narrow(short[] s, char[] c, byte[] b, int[] a) {
        long h = 59;
        for (int i = 0; i < s.length; i++) {
            long w = s[i];
            int v = s[i];
            h = mix(h, w);
            h = mix(h, v >>> 1);
            h = mix(h, v < 0 ? 1 : 0);
            h = mix(h, s[i] == -1 ? 3 : 4);
        }
        for (int i = 0; i < c.length; i++) {
            long w = c[i];
            int v = c[i];
            h = mix(h, w);
            h = mix(h, v >> 1);
            h = mix(h, (short) c[i]);
            h = mix(h, c[i] > 0x7FFF ? 5 : 6);
        }
        for (int i = 0; i < b.length; i++) {
            long w = b[i];
            int v = b[i];
            h = mix(h, w);
            h = mix(h, v & 0xFF);
            h = mix(h, b[i] < 0 ? 7 : 8);
        }
        // Store narrowing: an int written through each narrow array type reads back truncated.
        short[] ss = new short[1];
        char[] cc = new char[1];
        byte[] bb = new byte[1];
        boolean[] zz = new boolean[1];
        for (int i = 0; i < a.length; i++) {
            ss[0] = (short) a[i];
            cc[0] = (char) a[i];
            bb[0] = (byte) a[i];
            zz[0] = (a[i] & 1) != 0;
            h = mix(h, ss[0]);
            h = mix(h, cc[0]);
            h = mix(h, bb[0]);
            h = mix(h, zz[0] ? 1 : 0);
        }
        return h;
    }

    // ── FP min / max / abs / compares ──────────────────────────────────────────────────────

    static long minMaxD(double[] d) {
        long h = 61;
        for (int i = 0; i < d.length; i++) {
            for (int j = 0; j < d.length; j++) {
                double a = d[i], b = d[j];
                h = mix(h, Double.doubleToRawLongBits(Math.min(a, b)));
                h = mix(h, Double.doubleToRawLongBits(Math.max(a, b)));
            }
            h = mix(h, Double.doubleToRawLongBits(Math.min(PAYLOAD_NAN_D, d[i] == d[i] ? d[i] : 1.0)));
            h = mix(h, Double.doubleToRawLongBits(Math.max(d[i] == d[i] ? d[i] : 1.0, PAYLOAD_NAN_D)));
            h = mix(h, Double.doubleToRawLongBits(Math.abs(d[i])));
        }
        h = mix(h, Double.doubleToRawLongBits(Math.abs(PAYLOAD_NAN_D)));
        return h;
    }

    static long minMaxF(float[] f) {
        long h = 67;
        for (int i = 0; i < f.length; i++) {
            for (int j = 0; j < f.length; j++) {
                float a = f[i], b = f[j];
                h = mix(h, Float.floatToRawIntBits(Math.min(a, b)));
                h = mix(h, Float.floatToRawIntBits(Math.max(a, b)));
            }
            h = mix(h, Float.floatToRawIntBits(Math.min(PAYLOAD_NAN_F, f[i] == f[i] ? f[i] : 1.0f)));
            h = mix(h, Float.floatToRawIntBits(Math.max(f[i] == f[i] ? f[i] : 1.0f, PAYLOAD_NAN_F)));
            h = mix(h, Float.floatToRawIntBits(Math.abs(f[i])));
        }
        return h;
    }

    static long cmpD(double[] d) {
        long h = 71;
        for (int i = 0; i < d.length; i++) {
            for (int j = 0; j < d.length; j++) {
                double a = d[i], b = d[j];
                int bits = (a < b ? 1 : 0) | (a <= b ? 2 : 0) | (a == b ? 4 : 0) | (a != b ? 8 : 0)
                        | (a > b ? 16 : 0) | (a >= b ? 32 : 0) | (!(a < b) ? 64 : 0)
                        | (!(a >= b) ? 128 : 0) | (Double.compare(a, b) + 1 << 8);
                h = mix(h, bits);
            }
        }
        return h;
    }

    static long cmpF(float[] f) {
        long h = 73;
        for (int i = 0; i < f.length; i++) {
            for (int j = 0; j < f.length; j++) {
                float a = f[i], b = f[j];
                int bits = (a < b ? 1 : 0) | (a <= b ? 2 : 0) | (a == b ? 4 : 0) | (a != b ? 8 : 0)
                        | (a > b ? 16 : 0) | (a >= b ? 32 : 0) | (!(a < b) ? 64 : 0)
                        | (!(a >= b) ? 128 : 0) | (Float.compare(a, b) + 1 << 8);
                h = mix(h, bits);
            }
        }
        return h;
    }

    static long cmpLong(long[] l) {
        long h = 79;
        for (int i = 0; i < l.length; i++) {
            for (int j = 0; j < l.length; j++) {
                long a = l[i], b = l[j];
                int bits = (a < b ? 1 : 0) | (a <= b ? 2 : 0) | (a == b ? 4 : 0) | (a > b ? 8 : 0)
                        | (Long.compare(a, b) + 1 << 4) | (Long.compareUnsigned(a, b) + 1 << 6);
                h = mix(h, bits);
                if (b != 0) {
                    h = mix(h, Long.divideUnsigned(a, b));
                    h = mix(h, Long.remainderUnsigned(a, b));
                }
            }
        }
        return h;
    }

    static long cmpInt(int[] a) {
        long h = 83;
        for (int i = 0; i < a.length; i++) {
            for (int j = 0; j < a.length; j++) {
                int x = a[i], y = a[j];
                int bits = (x < y ? 1 : 0) | (x <= y ? 2 : 0) | (x == y ? 4 : 0) | (x > y ? 8 : 0)
                        | (Integer.compare(x, y) + 1 << 4) | (Integer.compareUnsigned(x, y) + 1 << 6)
                        | (Integer.toUnsignedLong(x) < Integer.toUnsignedLong(y) ? 256 : 0);
                h = mix(h, bits);
                h = mix(h, Math.min(x, y));
                h = mix(h, Math.max(x, y));
                if (y != 0) {
                    h = mix(h, Integer.divideUnsigned(x, y));
                    h = mix(h, Integer.remainderUnsigned(x, y));
                }
            }
        }
        return h;
    }

    // ── bit intrinsics ─────────────────────────────────────────────────────────────────────

    static long bitsInt(int[] a, int[] s) {
        long h = 89;
        for (int i = 0; i < a.length; i++) {
            int x = a[i];
            h = mix(h, Integer.reverse(x));
            h = mix(h, Integer.reverseBytes(x));
            h = mix(h, Integer.bitCount(x));
            h = mix(h, Integer.numberOfLeadingZeros(x));
            h = mix(h, Integer.numberOfTrailingZeros(x));
            h = mix(h, Integer.highestOneBit(x));
            h = mix(h, Integer.lowestOneBit(x));
            h = mix(h, Math.abs(x));
            h = mix(h, -x);
            h = mix(h, (long) Integer.reverse(x));
            h = mix(h, (long) Integer.highestOneBit(x));
            for (int j = 0; j < s.length; j++) {
                h = mix(h, Integer.rotateLeft(x, s[j]));
                h = mix(h, Integer.rotateRight(x, s[j]));
            }
        }
        return h;
    }

    static long bitsLong(long[] l, int[] s) {
        long h = 97;
        for (int i = 0; i < l.length; i++) {
            long x = l[i];
            h = mix(h, Long.reverse(x));
            h = mix(h, Long.reverseBytes(x));
            h = mix(h, Long.bitCount(x));
            h = mix(h, Long.numberOfLeadingZeros(x));
            h = mix(h, Long.numberOfTrailingZeros(x));
            h = mix(h, Long.highestOneBit(x));
            h = mix(h, Long.lowestOneBit(x));
            h = mix(h, Math.abs(x));
            h = mix(h, -x);
            for (int j = 0; j < l.length; j++) {
                h = mix(h, Math.multiplyHigh(x, l[j]));
                h = mix(h, Math.unsignedMultiplyHigh(x, l[j]));
                h = mix(h, x * l[j]);
            }
            for (int j = 0; j < s.length; j++) {
                h = mix(h, Long.rotateLeft(x, s[j]));
                h = mix(h, Long.rotateRight(x, s[j]));
            }
        }
        return h;
    }

    // ── FP remainder, rounding, sqrt ───────────────────────────────────────────────────────

    static long remFp(double[] d, float[] f) {
        long h = 101;
        for (int i = 0; i < d.length; i++) {
            for (int j = 0; j < d.length; j++) {
                h = mix(h, Double.doubleToLongBits(d[i] % d[j]));
            }
        }
        for (int i = 0; i < f.length; i++) {
            for (int j = 0; j < f.length; j++) {
                h = mix(h, Float.floatToIntBits(f[i] % f[j]));
            }
        }
        return h;
    }

    static long roundFp(double[] d, float[] f) {
        long h = 103;
        for (int i = 0; i < d.length; i++) {
            double x = d[i];
            h = mix(h, Double.doubleToLongBits(Math.floor(x)));
            h = mix(h, Double.doubleToLongBits(Math.ceil(x)));
            h = mix(h, Double.doubleToLongBits(Math.rint(x)));
            h = mix(h, Math.round(x));
            h = mix(h, Double.doubleToLongBits(Math.sqrt(x)));
            h = mix(h, Double.doubleToRawLongBits(-x));
            h = mix(h, Double.doubleToLongBits(x * -1.0));
            h = mix(h, Double.doubleToLongBits(0.0 - x));
        }
        for (int i = 0; i < f.length; i++) {
            float x = f[i];
            h = mix(h, Math.round(x));
            h = mix(h, Float.floatToRawIntBits(-x));
            h = mix(h, Float.floatToIntBits((float) Math.sqrt(x)));
        }
        return h;
    }

    static long fmaFp(double[] d) {
        long h = 107;
        for (int i = 0; i < d.length; i++) {
            for (int j = 0; j < d.length; j += 3) {
                h = mix(h, Double.doubleToLongBits(Math.fma(d[i], d[j], 0.1)));
                h = mix(h, Double.doubleToLongBits(d[i] * d[j] + 0.1));
            }
        }
        return h;
    }

    // ── raw FP bits (this lane's fix) ──────────────────────────────────────────────────────

    // Small enough to be spliced: `iload_0; i2l; lreturn`. A spliced `i2l` reads its argument's
    // whole 64-bit word.
    static long widen(int x) {
        return x;
    }

    static boolean lessThan(int x, int y) {
        return x < y;
    }

    static long rawBitsWiden(float[] f, double[] d) {
        long h = 109;
        for (int i = 0; i < f.length; i++) {
            float x = f[i];
            h = mix(h, widen(Float.floatToRawIntBits(x)));
            h = mix(h, (long) Float.floatToRawIntBits(x));
            h = mix(h, Float.floatToRawIntBits(x) >> 31);
            h = mix(h, lessThan(Float.floatToRawIntBits(x), -1) ? 1 : 2);
            h = mix(h, Float.floatToRawIntBits(Float.intBitsToFloat(Float.floatToRawIntBits(x))));
            h = mix(h, widen(Float.floatToRawIntBits(x * 2.0f)));
        }
        for (int i = 0; i < d.length; i++) {
            double x = d[i];
            h = mix(h, Double.doubleToRawLongBits(x));
            h = mix(h, Double.doubleToRawLongBits(Double.longBitsToDouble(Double.doubleToRawLongBits(x))));
            h = mix(h, widen((int) Double.doubleToRawLongBits(x)));
        }
        return h;
    }

    // ── overflow wrap ──────────────────────────────────────────────────────────────────────

    static long wrap(int[] a, long[] l) {
        long h = 113;
        for (int i = 0; i < a.length; i++) {
            int x = a[i];
            int y = x;
            y++;
            h = mix(h, widen(y));
            h = mix(h, widen(x + Integer.MAX_VALUE));
            h = mix(h, widen(x * 0x10001));
            h = mix(h, widen(x * -1));
            h = mix(h, (long) (x - Integer.MIN_VALUE));
            h = mix(h, (long) x * x);
            h = mix(h, (long) (x * x));
        }
        for (int i = 0; i < l.length; i++) {
            long x = l[i];
            h = mix(h, x + Long.MAX_VALUE);
            h = mix(h, x * 0x100000001L);
            h = mix(h, (int) (x * 31));
        }
        return h;
    }

    // ── harness ────────────────────────────────────────────────────────────────────────────

    interface Row {
        long run();
    }

    static void row(String name, Row r) {
        long cold = r.run();
        long hot = cold;
        for (int k = 0; k < WARM; k++) {
            hot = r.run();
            if (hot != cold) {
                break; // the first divergence is the answer; keep the line deterministic
            }
        }
        System.out.println("row " + name + " " + Long.toHexString(hot) + " stable " + (hot == cold));
    }

    public static void main(String[] args) {
        row("div-int", () -> divInt(INTS));
        row("div-long", () -> divLong(LONGS));
        row("div-const-int", () -> divConstInt(INTS));
        row("div-const-long", () -> divConstLong(LONGS));
        row("div-zero", () -> divZero(INTS));
        row("shift-var", () -> shiftVar(INTS, LONGS, SHIFTS));
        row("shift-const", () -> shiftConst(INTS, LONGS));
        row("conv-d", () -> convD(DOUBLES));
        row("conv-f", () -> convF(FLOATS));
        row("conv-int", () -> convInt(INTS, LONGS));
        row("narrow", () -> narrow(SHORTS, CHARS, BYTES, INTS));
        row("minmax-d", () -> minMaxD(MM_D));
        row("minmax-f", () -> minMaxF(MM_F));
        row("cmp-d", () -> cmpD(DOUBLES));
        row("cmp-f", () -> cmpF(FLOATS));
        row("cmp-long", () -> cmpLong(LONGS));
        row("cmp-int", () -> cmpInt(INTS));
        row("bits-int", () -> bitsInt(INTS, SHIFTS));
        row("bits-long", () -> bitsLong(LONGS, SHIFTS));
        row("rem-fp", () -> remFp(DOUBLES, FLOATS));
        row("round-fp", () -> roundFp(DOUBLES, FLOATS));
        row("fma-fp", () -> fmaFp(DOUBLES));
        row("rawbits-widen", () -> rawBitsWiden(FLOATS, DOUBLES));
        row("wrap", () -> wrap(INTS, LONGS));

        // A few single values, spelled out (after the warm-up above, so compiled callers).
        int[] m = {Integer.MIN_VALUE, -1};
        long[] ml = {Long.MIN_VALUE, -1L};
        System.out.println("fact int-min-div-m1 " + (m[0] / m[1]) + " rem " + (m[0] % m[1]));
        System.out.println("fact long-min-div-m1 " + (ml[0] / ml[1]) + " rem " + (ml[0] % ml[1]));
        double[] dd = {Double.NaN, 1e300, -1e300, -0.0};
        System.out.println("fact d2i " + (int) dd[0] + " " + (int) dd[1] + " " + (int) dd[2] + " " + (int) dd[3]);
        System.out.println("fact d2l " + (long) dd[0] + " " + (long) dd[1] + " " + (long) dd[2]);
        System.out.println("fact min-zero " + Double.doubleToRawLongBits(Math.min(0.0, dd[3]))
                + " max-zero " + Double.doubleToRawLongBits(Math.max(dd[3], 0.0)));
        float[] ff = {-1.0f};
        System.out.println("fact rawbits-widen " + widen(Float.floatToRawIntBits(ff[0])));
        int[] sh = {-1, 33};
        System.out.println("fact shifts " + (1 << sh[1]) + " " + (-8 >> sh[0]) + " " + (-8 >>> sh[0])
                + " " + (1L << sh[1]) + " " + (-8L >>> sh[0]));
        System.out.println("fact mulhi " + Math.multiplyHigh(ml[0], ml[0]) + " "
                + Math.unsignedMultiplyHigh(ml[1], ml[1]));
        System.out.println("fact ntz " + Long.numberOfTrailingZeros(0L) + " " + Long.numberOfTrailingZeros(ml[0])
                + " nlz " + Integer.numberOfLeadingZeros(0) + " rev " + Integer.reverse(1));
        System.out.println("done");
    }
}
