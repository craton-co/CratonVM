// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Adversarial cover for `boolean[]` offload, added 2026-09-05.
//
// `boolean[]` is the one array type whose STORE opcode does not identify
// it. `bastore` serves both `byte[]` and `boolean[]`, and JVMS 6.5 says
// the two do different things with the value:
//
//   byte[]    : store the low 8 bits
//   boolean[] : "the int value is narrowed by taking the bitwise AND
//               with 1"
//
// `caload`/`saload` let `char[]` share `short[]`'s ParamKind because the
// OPCODE carries the distinction; here it cannot, which is why the
// analyzer gives `boolean[]` a `ParamKind` of its own and the emitter
// masks on it.
//
// Legal javac output never puts a non-0/1 int into a `bastore` on a
// boolean array -- the language has no conversion that would -- so the
// mask is defensive against our own lowering rather than against Java.
// What these scenarios DO exercise end to end is the part that can
// silently rot: that a `boolean[]` marshals, survives a kernel, and
// writes back bit-exactly, sharing `byte[]`'s 1-byte path without
// picking up `byte[]`'s SIGN.
//
// Every checksum accumulates through `long` so a wrong value is visible
// rather than truncated away, and all of them are compared to HotSpot.
//
// Usage: java GpuBoolStress [scenario] [n]
//   0 all  1 identity  2 logic  3 mixed-with-byte  4 dense-alternating
public class GpuBoolStress {

    // The kernel shape the analyzer admits: static, void, counted loop.
    static void notZ(boolean[] in, boolean[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = !in[i];
        }
    }

    // Two inputs, one output: exercises a second boolean[] parameter and
    // a value that is computed rather than copied.
    static void andZ(boolean[] a, boolean[] b, boolean[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] & b[i];
        }
    }

    // boolean[] and byte[] side by side in ONE kernel. Both are 1 byte
    // per element and both go through the i8 marshal path, so if the
    // marshaller keyed on WIDTH rather than on element type it would
    // serve one of these from the other's device buffer. The byte values
    // deliberately include negatives, which a boolean can never hold.
    static void mixZB(boolean[] zIn, byte[] bIn, boolean[] zOut) {
        for (int i = 0; i < zOut.length; i++) {
            zOut[i] = zIn[i] ^ (bIn[i] < 0);
        }
    }

    static long mix(long h, long v) {
        return h * 1000003L + v;
    }

    // Accumulate as an INT so a boolean that is not exactly 0 or 1 shows.
    static long sumZ(boolean[] v) {
        long h = 0;
        for (boolean x : v) h = mix(h, x ? 1 : 0);
        return h;
    }

    static long sumB(byte[] v) {
        long h = 0;
        for (byte x : v) h = mix(h, x);
        return h;
    }

    static boolean[] mkZ(int n, int stride) {
        boolean[] v = new boolean[n];
        for (int i = 0; i < n; i++) v[i] = (i % stride) == 0;
        return v;
    }

    static byte[] mkB(int n) {
        byte[] v = new byte[n];
        // Spans the full signed range, so a sign-extension bug in the
        // shared i8 path shows up in `mixZB`.
        for (int i = 0; i < n; i++) v[i] = (byte) (i - 128);
        return v;
    }

    static long identity(int n) {
        boolean[] in = mkZ(n, 3), out = new boolean[n];
        notZ(in, out);
        return mix(sumZ(out), sumZ(in));
    }

    static long logic(int n) {
        boolean[] a = mkZ(n, 3), b = mkZ(n, 5), out = new boolean[n];
        andZ(a, b, out);
        return mix(mix(sumZ(out), sumZ(a)), sumZ(b));
    }

    static long mixed(int n) {
        boolean[] zIn = mkZ(n, 7), zOut = new boolean[n];
        byte[] bIn = mkB(n);
        mixZB(zIn, bIn, zOut);
        return mix(mix(sumZ(zOut), sumZ(zIn)), sumB(bIn));
    }

    // Every element true, then every element false, then alternating --
    // the shapes where a byte-wide store that failed to narrow would
    // still look right on a sparse input.
    static long dense(int n) {
        boolean[] all = new boolean[n], out = new boolean[n];
        for (int i = 0; i < n; i++) all[i] = true;
        notZ(all, out);
        long h = mix(sumZ(out), sumZ(all));
        boolean[] alt = mkZ(n, 2);
        notZ(alt, out);
        return mix(h, sumZ(out));
    }

    public static void main(String[] args) {
        int which = args.length > 0 ? Integer.parseInt(args[0]) : 0;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 131072;

        if (which == 0 || which == 1) System.out.println("identity=" + identity(n));
        if (which == 0 || which == 2) System.out.println("logic=" + logic(n));
        if (which == 0 || which == 3) System.out.println("mixed=" + mixed(n));
        if (which == 0 || which == 4) System.out.println("dense=" + dense(n));
        System.out.println("BOOL_DONE n=" + n);
    }
}
