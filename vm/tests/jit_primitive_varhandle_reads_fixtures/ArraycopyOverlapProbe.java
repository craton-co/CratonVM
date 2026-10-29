// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.reflect.Array;

public class ArraycopyOverlapProbe {
    static final Class<?>[] TYPES = {byte.class, boolean.class, short.class, char.class,
            int.class, long.class, float.class, double.class};
    static void copy(Object src, int from, Object dst, int to, int n) {
        System.arraycopy(src, from, dst, to, n);
    }
    static long copyAndSum(Object src, int from, Object dst, int to, int n, int kind) {
        System.arraycopy(src, from, dst, to, n);
        long sum = 0;
        for (int i = 0; i < n; i++) sum += elementBits(dst, kind, to + i);
        return sum;
    }
    static Object value(int kind, int i) {
        switch (kind) {
            case 0: return (byte) (i * 31 + 0x81);
            case 1: return (i & 1) != 0;
            case 2: return (short) (i * 977 + 0x8000);
            case 3: return (char) (i * 977 + 0x8000);
            case 4: return 0x81234567 + i;
            case 5: return 0x8123456789abcdefL + i;
            case 6: return Float.intBitsToFloat(0x7fc01234 + i);
            case 7: return Double.longBitsToDouble(0x7ff8000012345678L + i);
            default: throw new AssertionError(kind);
        }
    }
    // Typed loads avoid hundreds of thousands of temporary reflection boxes.
    // Preserve floating-point payload bits rather than numeric equality.
    static long elementBits(Object array, int kind, int i) {
        switch (kind) {
            case 0: return ((byte[]) array)[i];
            case 1: return ((boolean[]) array)[i] ? 1 : 0;
            case 2: return ((short[]) array)[i];
            case 3: return ((char[]) array)[i];
            case 4: return ((int[]) array)[i];
            case 5: return ((long[]) array)[i];
            case 6: return Float.floatToRawIntBits(((float[]) array)[i]);
            case 7: return Double.doubleToRawLongBits(((double[]) array)[i]);
            default: throw new AssertionError(kind);
        }
    }
    static void check(Object actual, int kind, long[] expected) {
        for (int i = 0; i < 80; i++) {
            if (elementBits(actual, kind, i) != expected[i])
                throw new AssertionError("copy mismatch kind=" + kind + " at " + i);
        }
    }
    interface Action { void run(); }
    static String failure(Action action) {
        try { action.run(); return "NONE"; }
        catch (Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }
    }
    public static void main(String[] args) {
        int cases = 0;
        for (int kind = 0; kind < TYPES.length; kind++) {
            Object source = Array.newInstance(TYPES[kind], 80);
            Object actual = Array.newInstance(TYPES[kind], 80);
            long[] expected = new long[80];
            for (int i = 0; i < 80; i++) Array.set(source, i, value(kind, i));
            long checksum = 0;
            for (int i = 0; i < 4; i++) checksum += elementBits(source, kind, i);
            for (int warm = 0; warm < 5000; warm++) {
                if (copyAndSum(source, 0, actual, 1, 4, kind) != checksum)
                    throw new AssertionError("copy checksum kind=" + kind);
            }
            for (int from : new int[]{0, 1, 3, 7, 15, 31})
                for (int to : new int[]{0, 1, 3, 7, 15, 31})
                    for (int n : new int[]{0, 1, 2, 3, 4, 7, 8, 9, 16, 32}) {
                        copy(source, 0, actual, 0, 80);
                        for (int i = 0; i < 80; i++) expected[i] = elementBits(source, kind, i);
                        // Snapshot semantics, including overlapping ranges.
                        for (int i = 0; i < n; i++) expected[to + i] = elementBits(source, kind, from + i);
                        copy(actual, from, actual, to, n);
                        check(actual, kind, expected);
                        cases++;
                    }
            System.gc();
        }
        for (int sk = 0; sk < TYPES.length; sk++) for (int dk = 0; dk < TYPES.length; dk++) {
            final Object src = Array.newInstance(TYPES[sk], 4);
            final Object dst = Array.newInstance(TYPES[dk], 4);
            System.out.println("types/" + sk + "/" + dk + "=" + failure(() -> copy(src, 0, dst, 0, 0)));
            System.out.println("types-bounds/" + sk + "/" + dk + "=" + failure(() -> copy(src, 0, dst, 0, 9)));
        }
        byte[] bytes = new byte[4];
        System.out.println("null-source=" + failure(() -> copy(null, 0, bytes, 0, 0)));
        System.out.println("null-dest=" + failure(() -> copy(bytes, 0, null, 0, 0)));
        System.out.println("nonarray=" + failure(() -> copy(new Object(), 0, bytes, 0, 0)));
        System.out.println("negative-length=" + failure(() -> copy(bytes, 0, bytes, 0, -1)));
        System.out.println("negative-source=" + failure(() -> copy(bytes, -1, bytes, 0, 1)));
        System.out.println("negative-dest=" + failure(() -> copy(bytes, 0, bytes, -1, 1)));
        Integer[] prefix = new Integer[3];
        System.out.println("reference-prefix=" + failure(() -> copy(new Object[] {1, "bad", 3}, 0, prefix, 0, 3)));
        System.out.println("prefix-state=" + java.util.Arrays.toString(prefix));
        System.out.println("ARRAYCOPY-OVERLAP-OK cases=" + cases);
    }
}
