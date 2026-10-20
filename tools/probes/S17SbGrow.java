// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// S17SbGrow: StringBuilder growth from compiled code, printed for a diff against HotSpot.
// Every append overload the JIT intrinsifies (int, char, String) is driven across the
// capacity boundary, from builders of capacity 0, 1, 16 and 37, with LATIN1 content, content
// that inflates the builder to UTF16 (char > 0xFF, a UTF16 String), null Strings, negative and
// extreme ints. capacity(), length(), the content's hash and the toString() are all observable.
public class S17SbGrow {
    static final String[] STRS = {"", "k", "abcdefghijklmnopq", "éÿ", "Σx", null};

    static long one(int cap, int seed) {
        StringBuilder sb = cap < 0 ? new StringBuilder() : new StringBuilder(cap);
        long h = 0;
        for (int k = 0; k < 24; k++) {
            int v = seed * 31 + k;
            switch ((seed + k) % 7) {
                case 0: sb.append(v); break;
                case 1: sb.append(-v); break;
                case 2: sb.append((k & 1) == 0 ? Integer.MIN_VALUE : Integer.MAX_VALUE); break;
                case 3: sb.append((char) ('a' + (v & 15))); break;
                case 4: sb.append((char) (0xF0 + (v & 31))); break;   // crosses 0xFF: inflates
                case 5: sb.append(STRS[(v & 0x7fffffff) % STRS.length]); break;
                default: sb.append(v % 3 == 0 ? (String) null : "zz"); break;
            }
            h = h * 31 + sb.capacity();
            h = h * 31 + sb.length();
        }
        return h * 31 + sb.toString().hashCode();
    }

    static long latin1Only(int cap, int seed) {
        // stays LATIN1: the served growth path only
        StringBuilder sb = cap < 0 ? new StringBuilder() : new StringBuilder(cap);
        long h = 0;
        for (int k = 0; k < 40; k++) {
            int v = seed + k * 7919;
            if ((k & 3) == 0) sb.append(v); else if ((k & 3) == 1) sb.append('-');
            else if ((k & 3) == 2) sb.append("ab"); else sb.append((String) null);
            h = h * 31 + sb.capacity();
        }
        return h * 31 + sb.toString().hashCode();
    }

    public static void main(String[] a) {
        int[] caps = {-1, 0, 1, 16, 37};
        for (int round = 0; round < 3; round++) {
            long s = 0, t = 0;
            for (int i = 0; i < 20_000; i++) {
                for (int cap : caps) {
                    s = s * 31 + one(cap, i);
                    t = t * 31 + latin1Only(cap, i);
                }
            }
            System.out.println("round " + round + " mixed=" + s + " latin1=" + t);
        }
        StringBuilder sb = new StringBuilder(0);
        for (int i = 0; i < 12; i++) { sb.append(i * 1000003); System.out.println(sb.capacity() + " " + sb.length() + " " + sb); }
        StringBuilder u = new StringBuilder("Σ");
        for (int i = 0; i < 12; i++) { u.append(i).append('x'); System.out.println(u.capacity() + " " + u); }
    }
}
