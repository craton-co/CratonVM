// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 109, lane L2 (assignment 3, the JIT correctness
// hunt): HotSpot semantics of the calls the JIT intrinsifies or fast-paths,
// each row run 20000 times so its lambda compiles (single-pass, then the
// optimizing tier) and every later answer must equal the first. A row whose
// compiled answer drifts prints `UNSTABLE <row> at <i>: <first> -> <now>`.
//
// Covered: Math.fma (fused rounding, overflow, NaN), Math.min/max (+-0.0,
// NaN), Math.abs edges, floor/ceil/rint of halves and -0.0, sqrt(-0.0),
// multiplyHigh / unsignedMultiplyHigh, Integer/Long bit intrinsics
// (rotate with negative / oversized distances, nlz of 0, highestOneBit),
// Integer/Long.compress/expand, Character.isDigit / digit / isLetter for
// supplementary code points, String.indexOf / lastIndexOf with supplementary
// and lone-surrogate needles and out-of-range fromIndex, Arrays.equals
// (null, ranges, range errors), Arrays.fill / sort on null, System.arraycopy
// (overlap both directions, every ArrayStoreException /
// ArrayIndexOutOfBoundsException text, null), and compiled array accesses'
// "Index N out of bounds for length L" texts.
//
// Fast-throw (wave 109 orchestrator hand-back): HotSpot's C2 replaces a hot
// IMPLICIT exception with a preallocated, message-less one
// (OmitStackTraceInFastThrow). Rows therefore never print an implicit
// exception's message raw: `array-index` maps a null message to the last real
// message of the same site and operands (`implicitText`); `arrays-null` and
// `arraycopy-null` print only the class (and a message that is null on every
// path). Every other message printed comes from an explicit `throw` or from
// the VM's arraycopy runtime, which fast-throw does not touch.
//
// Run: javac -d . L2W109CompiledIntrinsicSemantics.java
//      java -cp . L2W109CompiledIntrinsicSemantics                    (HotSpot)
//      cratonvm --jdk-only -cp . L2W109CompiledIntrinsicSemantics
//      cratonvm --compatible -cp . L2W109CompiledIntrinsicSemantics
//      (and with --nojit: the answers must be the same)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the javadoc / JDK sources)
// fma-d-fused 5.551115123125783E-17 unfused 0.0
// fma-d-no-overflow 1.7976931348623157E308
// fma-d-nan NaN 0.0
// fma-f-fused 1.4901161E-8 unfused 0.0
// minmax-d -0.0 0.0 -0.0 0.0 NaN NaN
// minmax-f -0.0 0.0 NaN 1.0
// abs -2147483648 -9223372036854775808 0.0 0.0 Infinity
// round-d -0.0 -0.0 2.0 -2.0 0.0 -0.0 -0.0
// mulhigh 4611686018427387903 0 -1 -2
// int-bits -2147483648 -2147483648 32 64 -2147483648 0 1073741824 -1
// long-bits -9223372036854775808 64 1 -9223372036854775808 0
// compress 11 176 255 61680 -1 0 0
// isdigit true false true 1 true -1 false
// indexof 1 1 2 1 -1 -1 2 -1 3 0
// arrays-equals true false true true false false
// arrays-equals-range AIOOBE Array index out of range: 5 | IAE fromIndex(3) > toIndex(1) | AIOOBE Array index out of range: -1
// arrays-null NPE NPE
// arraycopy-overlap [1, 1, 2, 3, 4] [2, 3, 4, 5, 5] [a, a, b]
// arraycopy-ase arraycopy: type mismatch: can not copy object array[] into int[]
// arraycopy-ase arraycopy: type mismatch: can not copy int[] into long[]
// arraycopy-ase arraycopy: type mismatch: can not copy int[] into object array[]
// arraycopy-ase arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, java.lang.Integer
// arraycopy-ase arraycopy: source type java.lang.String is not an array
// arraycopy-ase arraycopy: destination type java.lang.String is not an array
// arraycopy-partial [7, 0]
// arraycopy-aioobe arraycopy: last source index 13 out of bounds for int[10]
// arraycopy-aioobe arraycopy: last destination index 5 out of bounds for int[3]
// arraycopy-aioobe arraycopy: source index -1 out of bounds for int[10]
// arraycopy-aioobe arraycopy: destination index -2 out of bounds for char[4]
// arraycopy-aioobe arraycopy: length -1 is negative
// arraycopy-aioobe arraycopy: last destination index 4 out of bounds for object array[3]
// arraycopy-aioobe arraycopy: last source index 2147483648 out of bounds for byte[4]
// arraycopy-null java.lang.NullPointerException null
// array-index Index 5 out of bounds for length 3 | Index -1 out of bounds for length 3 | Index 3 out of bounds for length 3 | Index 2147483647 out of bounds for length 0
// HOTSPOT_EXPECTED_END
import java.util.Arrays;

public class L2W109CompiledIntrinsicSemantics {
    interface Row {
        String run(int i) throws Throwable;
    }

    static final int RUNS = 20000;

    static void check(String name, Row row) throws Throwable {
        String first = row.run(0);
        for (int i = 1; i < RUNS; i++) {
            String now = row.run(i);
            if (!now.equals(first)) {
                System.out.println("UNSTABLE " + name + " at " + i + ": " + first + " -> " + now);
                break;
            }
        }
        System.out.println(name + " " + first);
    }

    // Inputs read from arrays so no row folds to a compile-time constant.
    static final double[] D = {0.1, 10.0, -1.0, Double.MAX_VALUE, 2.0, 0.0, Double.POSITIVE_INFINITY, 1.0, -0.0, Double.NaN};
    static final float[] F = {0.1f, 10.0f, -1.0f, 0.0f, -0.0f, Float.NaN, 1.0f};
    static final int[] I = {Integer.MIN_VALUE, -1, 0, 1, 0b10110100, 0b11110000, 11, 5};
    static final long[] L = {Long.MIN_VALUE, -1L, 0L, 1L, Long.MAX_VALUE, 0xF0F0L, 0xFFL, 2L};

    static String aseText(Object src, Object dst) {
        try {
            System.arraycopy(src, 0, dst, 0, 1);
            return "no exception";
        } catch (ArrayStoreException e) {
            return e.getMessage();
        }
    }

    static String aioobeText(Object src, int sp, Object dst, int dp, int len) {
        try {
            System.arraycopy(src, sp, dst, dp, len);
            return "no exception";
        } catch (ArrayIndexOutOfBoundsException e) {
            return e.getMessage();
        }
    }

    // HotSpot's C2 turns a hot implicit exception into a preallocated one with
    // a null message (OmitStackTraceInFastThrow, on by default). The probe must
    // not depend on that: a null message from an implicit throw is answered
    // with the last real message the same site and operands produced, so a
    // fast-thrown exception reads as its earlier, messaged self on every VM.
    // A VM whose FIRST throw already has no message still prints "null".
    static final java.util.HashMap<String, String> LAST_MESSAGE = new java.util.HashMap<>();

    static String implicitText(String key, RuntimeException e) {
        String m = e.getMessage();
        if (m == null) {
            return String.valueOf(LAST_MESSAGE.get(key));
        }
        LAST_MESSAGE.put(key, m);
        return m;
    }

    static String indexText(int[] a, int idx) {
        try {
            return "value " + a[idx];
        } catch (ArrayIndexOutOfBoundsException e) {
            return implicitText("iaload " + idx + " " + a.length, e);
        }
    }

    static String storeText(long[] a, int idx) {
        try {
            a[idx] = 1L;
            return "stored";
        } catch (ArrayIndexOutOfBoundsException e) {
            return implicitText("lastore " + idx + " " + a.length, e);
        }
    }

    static String charText(char[] a, int idx) {
        try {
            return "value " + (int) a[idx];
        } catch (ArrayIndexOutOfBoundsException e) {
            return implicitText("caload " + idx + " " + a.length, e);
        }
    }

    public static void main(String[] args) throws Throwable {
        check("fma-d-fused", i -> Math.fma(D[0], D[1], D[2]) + " unfused " + (D[0] * D[1] + D[2]));
        check("fma-d-no-overflow", i -> String.valueOf(Math.fma(D[3], D[4], -D[3])));
        check("fma-d-nan", i -> Math.fma(D[5], D[6], D[7]) + " " + Math.fma(D[8], D[5], D[5]));
        check("fma-f-fused", i -> Math.fma(F[0], F[1], F[2]) + " unfused " + (F[0] * F[1] + F[2]));
        check("minmax-d", i -> Math.min(D[5], D[8]) + " " + Math.max(D[8], D[5]) + " "
                + Math.min(D[8], D[5]) + " " + Math.max(D[5], D[8]) + " "
                + Math.min(D[7], D[9]) + " " + Math.max(D[9], D[7]));
        check("minmax-f", i -> Math.min(F[3], F[4]) + " " + Math.max(F[4], F[3]) + " "
                + Math.max(F[5], F[6]) + " " + Math.min(F[6], F[6]));
        check("abs", i -> Math.abs(I[0]) + " " + Math.abs(L[0]) + " " + Math.abs(D[8]) + " "
                + Math.abs(F[4]) + " " + Math.abs(-D[6]));
        check("round-d", i -> Math.ceil(-0.25 + D[5]) + " " + Math.ceil(-0.5 + D[5]) + " "
                + Math.rint(2.5 + D[5]) + " " + Math.rint(-2.5 + D[5]) + " "
                + Math.rint(0.49999999999999994 + D[5]) + " " + Math.sqrt(D[8]) + " "
                + Math.floor(D[8]));
        check("mulhigh", i -> Math.multiplyHigh(L[4], L[4]) + " " + Math.multiplyHigh(L[1], L[1]) + " "
                + Math.multiplyHigh(L[0], L[7]) + " " + Math.unsignedMultiplyHigh(L[1], L[1]));
        check("int-bits", i -> Integer.rotateLeft(I[3], -1) + " " + Integer.rotateRight(I[3], 33) + " "
                + Integer.numberOfLeadingZeros(I[2]) + " " + Long.numberOfTrailingZeros(L[2]) + " "
                + Integer.highestOneBit(I[1]) + " " + Integer.lowestOneBit(I[2]) + " "
                + Integer.reverse(I[3] << 1) + " " + Integer.compare(I[0], I[3]));
        check("long-bits", i -> Long.rotateRight(L[3], 65) + " " + Long.bitCount(L[1]) + " "
                + Long.compare(L[4], L[0]) + " " + Long.highestOneBit(L[1]) + " "
                + Long.lowestOneBit(L[2]));
        check("compress", i -> Integer.compress(I[4], I[5]) + " " + Integer.expand(I[6], I[5]) + " "
                + Long.compress(L[1], L[5]) + " " + Long.expand(L[6], L[5]) + " "
                + Integer.compress(I[1], I[1]) + " " + Integer.expand(I[7], I[2]) + " "
                + Long.compress(L[1], L[2]));
        check("isdigit", i -> Character.isDigit(0x1D7CE + I[2]) + " " + Character.isDigit(0x10000 + I[2]) + " "
                + Character.isDigit(0x0660 + I[2]) + " " + Character.digit(0x1D7CF + I[2], 10) + " "
                + Character.isLetter(0x1D400 + I[2]) + " " + Character.digit(0x1D7CF + I[2], 1) + " "
                + Character.isDigit(-1 + I[2]));
        String s = "a😀b";
        check("indexof", i -> s.indexOf(0x1F600 + I[2]) + " " + s.indexOf(0xD83D + I[2]) + " "
                + s.indexOf(0xDE00 + I[2]) + " " + s.lastIndexOf(0x1F600 + I[2]) + " "
                + s.indexOf(0x110000 + I[2]) + " " + s.indexOf(-1 + I[2]) + " "
                + "abc".indexOf('c', -5 + I[2]) + " " + "abc".indexOf('a', 10 + I[2]) + " "
                + s.indexOf('b', 1 + I[2]) + " " + s.indexOf("a\uD83D") );
        int[] a4 = {1, 2, 3, 4};
        int[] b3 = {9, 2, 3};
        check("arrays-equals", i -> {
            int[] n = null;
            return Arrays.equals(n, n) + " " + Arrays.equals(a4, n) + " "
                    + Arrays.equals(new int[0], new int[I[2]]) + " "
                    + Arrays.equals(a4, 1, 3, b3, 1, 3) + " "
                    + Arrays.equals(a4, 0, 3, b3, 0, 3) + " "
                    + Arrays.equals(a4, 1, 3, b3, 1, 2);
        });
        check("arrays-equals-range", i -> {
            StringBuilder sb = new StringBuilder();
            try {
                Arrays.equals(a4, 0, 5 + I[2], b3, 0, 3);
            } catch (ArrayIndexOutOfBoundsException e) {
                sb.append("AIOOBE ").append(e.getMessage());
            }
            try {
                Arrays.equals(a4, 3, 1, b3, 0, 2);
            } catch (IllegalArgumentException e) {
                sb.append(" | IAE ").append(e.getMessage());
            }
            try {
                Arrays.equals(a4, -1 + I[2], 2, b3, 0, 3);
            } catch (ArrayIndexOutOfBoundsException e) {
                sb.append(" | AIOOBE ").append(e.getMessage());
            }
            return sb.toString();
        });
        check("arrays-null", i -> {
            int[] n = null;
            String out = "";
            try {
                Arrays.fill(n, 1);
            } catch (NullPointerException e) {
                out += "NPE";
            }
            try {
                Arrays.sort(n);
            } catch (NullPointerException e) {
                out += " NPE";
            }
            return out;
        });
        check("arraycopy-overlap", i -> {
            int[] up = {1, 2, 3, 4, 5};
            System.arraycopy(up, 0, up, 1 + I[2], 4);
            int[] down = {1, 2, 3, 4, 5};
            System.arraycopy(down, 1 + I[2], down, 0, 4);
            String[] refs = {"a", "b", "c"};
            System.arraycopy(refs, 0, refs, 1 + I[2], 2);
            return Arrays.toString(up) + " " + Arrays.toString(down) + " " + Arrays.toString(refs);
        });
        check("arraycopy-ase", i -> aseText(new Object[2], new int[2]));
        check("arraycopy-ase", i -> aseText(new int[2], new long[2]));
        check("arraycopy-ase", i -> aseText(new int[2], new Object[2]));
        check("arraycopy-ase", i -> aseText(new Object[] {"s"}, new Integer[1]));
        check("arraycopy-ase", i -> aseText("s", new int[1]));
        check("arraycopy-ase", i -> aseText(new int[1], "s"));
        check("arraycopy-partial", i -> {
            // The element that fits is stored before the one that does not.
            Object[] src = {Integer.valueOf(7), "x"};
            Integer[] dst = new Integer[2];
            try {
                System.arraycopy(src, 0, dst, 0, 2);
            } catch (ArrayStoreException e) {
                // expected
            }
            return "[" + dst[0] + ", " + (dst[1] == null ? 0 : dst[1]) + "]";
        });
        check("arraycopy-aioobe", i -> aioobeText(new int[10], 5 + I[2], new int[10], 0, 8));
        check("arraycopy-aioobe", i -> aioobeText(new int[10], 0, new int[3], 0, 5 + I[2]));
        check("arraycopy-aioobe", i -> aioobeText(new int[10], -1 + I[2], new int[10], 0, 1));
        check("arraycopy-aioobe", i -> aioobeText(new char[4], 0, new char[4], -2 + I[2], 1));
        check("arraycopy-aioobe", i -> aioobeText(new int[10], 0, new int[10], 0, -1 + I[2]));
        check("arraycopy-aioobe", i -> aioobeText(new Object[3], 0, new Object[3], 1 + I[2], 3));
        check("arraycopy-aioobe", i -> aioobeText(new byte[4], 1 + I[2], new byte[4], 0, Integer.MAX_VALUE));
        check("arraycopy-null", i -> {
            try {
                System.arraycopy(null, 0, new int[1], 0, 1);
                return "no exception";
            } catch (NullPointerException e) {
                return e.getClass().getName() + " " + e.getMessage();
            }
        });
        int[] a3 = new int[3];
        long[] l3 = new long[3];
        char[] c0 = new char[0];
        check("array-index", i -> indexText(a3, 5 + I[2]) + " | " + indexText(a3, -1 + I[2]) + " | "
                + storeText(l3, 3 + I[2]) + " | " + charText(c0, Integer.MAX_VALUE + I[2]));
    }
}
