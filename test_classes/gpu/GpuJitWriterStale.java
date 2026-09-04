// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Can a JIT-COMPILED writer leave the GPU residency cache stale?
//
// `offload_jit_gate` exists to stop exactly that. Under the default
// policy, a method that stores into a primitive array of a type the
// input cache can hold is refused JIT compilation, so its stores stay on
// an interpreter path that evicts the cache entry. The scan that decides
// this looks for `iastore`/`lastore`/`fastore`/`dastore` (0x4f..=0x52).
//
// It does NOT look for `bastore` (0x54) or `sastore` (0x56). That was
// harmless while short[] and byte[] could not be offloaded -- nothing
// cached them, so nothing could go stale. Since 2026-09-02 they ARE
// cached, and the scan has not moved: a `short[]`/`byte[]` writer is
// still admitted to the JIT while its array may be device-resident.
//
// This fixture is built so the mutation happens inside its OWN method,
// hot enough to be compiled, rather than inline in the submitting loop:
// `bump*` is what the gate does or does not refuse.
//
// The int[] arm is the CONTROL. `bumpI` contains `iastore`, so the gate
// refuses it, and the int[] result must be correct. If short/byte
// diverge while int does not, the gate's opcode set is the difference
// and nothing else is.
//
// Usage: java GpuJitWriterStale [n] [rounds]
public class GpuJitWriterStale {

    static void scaleS(short[] in, short[] out) {
        for (int i = 0; i < out.length; i++) out[i] = (short) (in[i] * 3 - 7);
    }

    static void scaleB(byte[] in, byte[] out) {
        for (int i = 0; i < out.length; i++) out[i] = (byte) (in[i] * 3 - 7);
    }

    static void scaleC(char[] in, char[] out) {
        for (int i = 0; i < out.length; i++) out[i] = (char) (in[i] * 3 - 7);
    }

    static void scaleI(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) out[i] = in[i] * 3 - 7;
    }

    // The writers under test. Each is a separate method containing
    // exactly one kind of array store, called often enough to be a JIT
    // candidate in its own right.
    static void bumpS(short[] a, int r) {
        for (int i = 0; i < a.length; i += 64) a[i] = (short) (a[i] + r + 1);
    }

    static void bumpB(byte[] a, int r) {
        for (int i = 0; i < a.length; i += 64) a[i] = (byte) (a[i] + r + 1);
    }

    static void bumpC(char[] a, int r) {
        for (int i = 0; i < a.length; i += 64) a[i] = (char) (a[i] + r + 1);
    }

    static void bumpI(int[] a, int r) {
        for (int i = 0; i < a.length; i += 64) a[i] = a[i] + r + 1;
    }

    static long mix(long h, long v) {
        return h * 1000003L + v;
    }

    static long sumS(short[] v) {
        long h = 0;
        for (short x : v) h = mix(h, x);
        return h;
    }

    static long sumB(byte[] v) {
        long h = 0;
        for (byte x : v) h = mix(h, x);
        return h;
    }

    static long sumC(char[] v) {
        long h = 0;
        for (char x : v) h = mix(h, x);
        return h;
    }

    static long sumI(int[] v) {
        long h = 0;
        for (int x : v) h = mix(h, x);
        return h;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 131072;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;

        short[] sIn = new short[n], sOut = new short[n];
        byte[] bIn = new byte[n], bOut = new byte[n];
        int[] iIn = new int[n], iOut = new int[n];
        char[] cIn = new char[n], cOut = new char[n];
        for (int i = 0; i < n; i++) {
            sIn[i] = (short) (i % 1013);
            bIn[i] = (byte) (i % 113);
            iIn[i] = i % 1013;
            cIn[i] = (char) (i % 1013);
        }

        long hs = 0, hb = 0, hi = 0, hc = 0;
        for (int r = 0; r < rounds; r++) {
            // Submit: the input becomes device-resident.
            scaleS(sIn, sOut);
            scaleB(bIn, bOut);
            scaleI(iIn, iOut);
            scaleC(cIn, cOut);
            hs = mix(hs, sumS(sOut));
            hb = mix(hb, sumB(bOut));
            hi = mix(hi, sumI(iOut));
            hc = mix(hc, sumC(cOut));

            // Mutate through a compiled writer. If the cache entry is
            // not evicted, the NEXT submit computes from the device copy
            // taken before this ran.
            bumpS(sIn, r);
            bumpB(bIn, r);
            bumpI(iIn, r);
            bumpC(cIn, r);
        }

        System.out.println("jit_writer_short=" + mix(hs, sumS(sIn)));
        System.out.println("jit_writer_byte=" + mix(hb, sumB(bIn)));
        System.out.println("jit_writer_int=" + mix(hi, sumI(iIn)));
        System.out.println("jit_writer_char=" + mix(hc, sumC(cIn)));
        System.out.println("JIT_WRITER_DONE n=" + n + " rounds=" + rounds);
    }
}
