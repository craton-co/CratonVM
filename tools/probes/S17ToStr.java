// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// S17ToStr: StringBuilder.toString() from compiled code, printed for a diff against HotSpot.
// Empty builders (the "" literal: identity is observable), LATIN1 and UTF16 content, a builder
// mutated after toString (the String must be a copy), sizes from 0 to past a TLAB, setLength,
// interning, and the String's own hashCode/equals/length/charAt/coder-visible behaviour.
public class S17ToStr {
    static long one(int seed) {
        long h = 0;
        StringBuilder sb = new StringBuilder(seed % 5 == 0 ? 0 : 16);
        String e = sb.toString();
        h = h * 31 + (e == "" ? 1 : 2);                 // JDK 25: the literal itself
        int n = seed % 41;
        for (int k = 0; k < n; k++) sb.append((char) ('a' + ((seed + k) % 26)));
        String s = sb.toString();
        h = h * 31 + s.length();
        h = h * 31 + s.hashCode();
        sb.append('Z');                                 // must not leak into s
        h = h * 31 + s.length();
        h = h * 31 + (s.equals(sb.substring(0, n)) ? 7 : 9);
        if (n > 0) {
            sb.setCharAt(0, '#');                       // must not leak into s either
            h = h * 31 + s.charAt(0);
        }
        sb.setLength(0);
        String z = sb.toString();
        h = h * 31 + (z == "" ? 3 : 4);
        sb.append(seed).append('-').append(-seed);
        String t = sb.toString();
        h = h * 31 + t.hashCode() + (t.intern() == t.intern() ? 1 : 0);
        if ((seed & 7) == 0) {
            sb.append('Σ');                        // inflates: the UTF16 arm declines
            String u = sb.toString();
            h = h * 31 + u.hashCode() + u.length() + u.charAt(u.length() - 1);
        }
        return h;
    }

    static long big(int len) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < len; i++) sb.append((char) ('0' + i % 10));
        String s = sb.toString();
        sb.setLength(len / 2);
        return (long) s.length() * 31 + s.hashCode() + s.charAt(len - 1) + sb.toString().length();
    }

    public static void main(String[] a) {
        for (int round = 0; round < 3; round++) {
            long s = 0;
            for (int i = 0; i < 200_000; i++) s = s * 31 + one(i);
            System.out.println("round " + round + " " + s);
        }
        long b = 0;
        for (int r = 0; r < 200; r++) for (int len : new int[] {1, 15, 16, 17, 255, 4096, 70_000, 300_000}) b = b * 31 + big(len);
        System.out.println("big " + b);
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 6; i++) { sb.append(i); String x = sb.toString(); System.out.println(x + " " + x.length() + " " + x.hashCode() + " " + (x == sb.toString())); }
        System.out.println(new StringBuilder().toString() == "");
        System.out.println(new StringBuilder("Σ").toString().length());
    }
}
