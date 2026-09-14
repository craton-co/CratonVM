// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Adversarial cover for `char[]` offload, added 2026-09-03.
//
// `char` is the only UNSIGNED 16-bit type in the JVM, and that is the
// whole risk. `caload` zero-extends where `saload` sign-extends, and the
// two share a store (`castore`/`sastore` are both a 16-bit truncating
// store). So a `char[]` marshalled or lowered as if it were a `short[]`
// is bit-identical on the way out and WRONG on the way in, for exactly
// the half of the range above 0x7FFF.
//
// Every scenario therefore drives values across 0x8000, where a sign
// extension and a zero extension disagree:
//
//   'A'      0x0041  same either way
//   0x7FFF   32767   the last value they agree on
//   0x8000   32768   sign-extends NEGATIVE, zero-extends positive
//   0xFFFF   65535   sign-extends to -1
//
// `sumC` accumulates through `long` so the difference is visible rather
// than truncated away, and the checksums are compared against HotSpot.
//
// Usage: java GpuCharStress [scenario] [n]
//   0 all  1 identity  2 arithmetic  3 full-range  4 mixed-with-short
public class GpuCharStress {

    // The kernel shape the analyzer admits: static, void, counted loop.
    static void bumpC(char[] in, char[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (char) (in[i] + 1);
        }
    }

    // Multiplication pushes low values up past 0x8000, so a run that
    // only ever sees small chars would still cross the boundary here.
    static void scaleC(char[] in, char[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (char) (in[i] * 3 + 7);
        }
    }

    // char and short side by side in one kernel: if the marshaller keyed
    // on width rather than on element type it would serve one of these
    // from the other's device buffer.
    static void mixCS(char[] cIn, short[] sIn, char[] cOut) {
        for (int i = 0; i < cOut.length; i++) {
            cOut[i] = (char) (cIn[i] + sIn[i]);
        }
    }

    static long mix(long h, long v) {
        return h * 1000003L + v;
    }

    // Accumulate the char as an INT, which is where zero-extension shows.
    static long sumC(char[] v) {
        long h = 0;
        for (char x : v) h = mix(h, x);
        return h;
    }

    static long sumS(short[] v) {
        long h = 0;
        for (short x : v) h = mix(h, x);
        return h;
    }

    static char[] mkC(int n) {
        char[] v = new char[n];
        char[] edge = {
            0, 1, 'A', 0x7FFE, 0x7FFF, 0x8000, 0x8001, 0xFFFE, 0xFFFF,
        };
        for (int i = 0; i < n; i++) {
            v[i] = (i % 11 == 0) ? edge[i % edge.length] : (char) (i * 7 + 3);
        }
        return v;
    }

    static short[] mkS(int n) {
        short[] v = new short[n];
        for (int i = 0; i < n; i++) v[i] = (short) (i - 32768);
        return v;
    }

    static long identity(int n) {
        char[] in = mkC(n), out = new char[n];
        bumpC(in, out);
        return mix(sumC(out), sumC(in));
    }

    static long arithmetic(int n) {
        char[] in = mkC(n), out = new char[n];
        scaleC(in, out);
        return mix(sumC(out), sumC(in));
    }

    // Every one of the 65536 char values, so no extension can hide.
    static long fullRange() {
        int n = 65536;
        char[] in = new char[n], out = new char[n];
        for (int i = 0; i < n; i++) in[i] = (char) i;
        scaleC(in, out);
        long h = mix(sumC(out), sumC(in));
        bumpC(in, out);
        return mix(h, sumC(out));
    }

    static long mixed(int n) {
        char[] cIn = mkC(n), cOut = new char[n];
        short[] sIn = mkS(n);
        mixCS(cIn, sIn, cOut);
        return mix(mix(sumC(cOut), sumC(cIn)), sumS(sIn));
    }

    public static void main(String[] args) {
        int which = args.length > 0 ? Integer.parseInt(args[0]) : 0;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 131072;

        if (which == 0 || which == 1) System.out.println("identity=" + identity(n));
        if (which == 0 || which == 2) System.out.println("arithmetic=" + arithmetic(n));
        if (which == 0 || which == 3) System.out.println("full_range=" + fullRange());
        if (which == 0 || which == 4) System.out.println("mixed=" + mixed(n));
        System.out.println("CHAR_DONE n=" + n);
    }
}
