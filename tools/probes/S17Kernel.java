// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// S17Kernel <mode> : the R16Bench strings body, timed four ways (ns per iteration).
//   osr    : one 2M-iteration loop entered once (R16Bench.strings' shape: an OSR body)
//   chunk  : the same loop body in a method called 2 000 times x 1 000 iterations (a normal body)
//   nohash : osr without the trailing hashCode (does the String intrinsic pin decide it?)
//   len    : osr with length() instead of hashCode()
public class S17Kernel {
    static long osr(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { StringBuilder sb = new StringBuilder(); sb.append("k").append(i).append('-').append(i * 7); s += sb.toString().hashCode(); }
        return s;
    }
    static long nohash(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { StringBuilder sb = new StringBuilder(); sb.append("k").append(i).append('-').append(i * 7); s += sb.toString().charAt(1); }
        return s;
    }
    static long len(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { StringBuilder sb = new StringBuilder(); sb.append("k").append(i).append('-').append(i * 7); s += sb.toString().length(); }
        return s;
    }
    static long chunkBody(int from, int to) {
        long s = 0;
        for (int i = from; i < to; i++) { StringBuilder sb = new StringBuilder(); sb.append("k").append(i).append('-').append(i * 7); s += sb.toString().hashCode(); }
        return s;
    }
    static long cap32Body(int from, int to) {
        long s = 0;
        for (int i = from; i < to; i++) { StringBuilder sb = new StringBuilder(32); sb.append("k").append(i).append('-').append(i * 7); s += sb.toString().hashCode(); }
        return s;
    }
    static long cap32(int n) {
        long s = 0;
        for (int c = 0; c < n; c += 1000) s += cap32Body(c, c + 1000);
        return s;
    }
    static long small(int n) {
        // chunk's body over i < 100 000 only (strings <= 14 chars): never grows
        long s = 0;
        for (int r = 0; r < n / 100000; r++) for (int c = 0; c < 100000; c += 1000) s += chunkBody(c, c + 1000);
        return s;
    }
    static long growBody(int from, int to, int cap) {
        long s = 0;
        for (int i = from; i < to; i++) { StringBuilder sb = new StringBuilder(cap); sb.append(1000000 + i); s += sb.length(); }
        return s;
    }
    static long grow(int n, int cap) {
        long s = 0;
        for (int c = 0; c < n; c += 1000) s += growBody(c, c + 1000, cap);
        return s;
    }
    static long chunk(int n) {
        long s = 0;
        for (int c = 0; c < n; c += 1000) s += chunkBody(c, c + 1000);
        return s;
    }
    static long run(String m, int n) {
        switch (m) {
            case "osr": return osr(n);
            case "chunk": return chunk(n);
            case "nohash": return nohash(n);
            case "len": return len(n);
            case "cap32": return cap32(n);
            case "grow": return grow(n, 1);
            case "nogrow": return grow(n, 16);
            case "small": return small(n);
            default: throw new IllegalArgumentException(m);
        }
    }
    public static void main(String[] a) {
        String m = a[0];
        int n = a.length > 1 ? Integer.parseInt(a[1]) : 2_000_000;
        long r = run(m, n);
        long t0 = System.nanoTime();
        r += run(m, n);
        long t1 = System.nanoTime();
        System.out.println(m + " = " + r + " : " + ((t1 - t0) / n) + " ns/op");
    }
}
