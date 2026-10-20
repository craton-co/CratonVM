// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;

// S17Hash: hashes computed in hot (compiled) loops, printed for a diff against HotSpot.
// Covers every shape ArraysSupport.vectorizedHashCode is reached with: String (LATIN1 via
// hashCodeOfUnsigned / T_BOOLEAN, UTF16 via T_CHAR over byte[] pairs), Arrays.hashCode of
// byte[] (T_BYTE, signed), char[] (T_CHAR), short[] (T_SHORT), int[] (T_INT), empty and
// one-element arrays, and a null array (Arrays.hashCode(null) == 0).
public class S17Hash {
    static long latin1(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            StringBuilder sb = new StringBuilder();
            sb.append("k").append(i).append('é').append(i * 7);
            s = s * 31 + sb.toString().hashCode();
        }
        return s;
    }
    static long utf16(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            StringBuilder sb = new StringBuilder();
            sb.append('Σ').append(i).append("ΟΣ").append(-i);
            s = s * 31 + sb.toString().hashCode();
        }
        return s;
    }
    static long arrays(int n) {
        long s = 0;
        byte[] b = new byte[37];
        char[] c = new char[19];
        short[] h = new short[23];
        int[] w = new int[11];
        for (int i = 0; i < n; i++) {
            b[i % b.length] = (byte) (i * 13);
            c[i % c.length] = (char) (i * 977);
            h[i % h.length] = (short) (-i * 31);
            w[i % w.length] = i * 0x9E3779B9;
            s = s * 31 + Arrays.hashCode(b);
            s = s * 31 + Arrays.hashCode(c);
            s = s * 31 + Arrays.hashCode(h);
            s = s * 31 + Arrays.hashCode(w);
            s = s * 31 + Arrays.hashCode(new byte[i & 1]);
            s = s * 31 + Arrays.hashCode((byte[]) null);
        }
        return s;
    }
    static long cached(int n) {
        // A string hashed once and then again: the cache must hold the same value.
        long s = 0;
        for (int i = 0; i < n; i++) {
            String t = new String(new char[] {'a', (char) ('a' + (i & 15)), 'Ā'});
            int first = t.hashCode();
            int second = t.hashCode();
            if (first != second) throw new AssertionError(t);
            s = s * 31 + first;
        }
        return s;
    }
    public static void main(String[] a) {
        int n = 300_000;
        for (int round = 0; round < 3; round++) {
            System.out.println("latin1 " + latin1(n));
            System.out.println("utf16 " + utf16(n));
            System.out.println("arrays " + arrays(n / 3));
            System.out.println("cached " + cached(n));
        }
        System.out.println("ΣΟΣ".hashCode() + " " + "".hashCode() + " " + "k1999999-13999993".hashCode());
    }
}
