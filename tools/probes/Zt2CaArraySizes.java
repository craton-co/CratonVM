// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t2/ca (2026-10-19): the footprint of every RUN-TIME-length compiled
// array allocation, checked from inside the hot loop that allocates it.
//
// `CRATONVM_JIT_ZGC_NEWARRAY_SIZE_OFF_CHAIN` (and zs s10/ca5's
// `CRATONVM_JIT_ZGC_NEWARRAY_SKIP_WORD_ROUND`) recompute the inline bump's
// footprint `16 + round_up_8(length << log2)` in a different instruction
// sequence. A footprint that came out too SMALL makes the next array's
// header land inside this one's tail; one too LARGE leaves a gap the heap
// walker reads as an object. Both show here:
//
//   - each burst allocates 64 arrays of one element type back to back, with
//     lengths read from an int[] (never constants: the run-time arm), every
//     length 0..47 plus the three lengths around the inline cap of that
//     type (`(32768 - 16) / element size`: below, at, and past it, which the
//     inline screen sends to the stub);
//   - every array is checked for its length and a zero body, then its whole
//     body (or, past 64 elements, its first and last element) is set to a
//     sentinel; after the NEXT allocation the previous array's sentinel must
//     still be there (a too-small footprint lets the next header overwrite
//     it);
//   - the arrays escape into a static ring, so escape analysis cannot remove
//     the allocation;
//   - a negative run-time length raises NegativeArraySizeException with
//     HotSpot's message (the length).
//
// The checks run inside the burst methods, so the compiled bodies check
// themselves (a check in `main` alone would measure the interpreter). The
// heap walker sees a wrong footprint at the next collection: run with a small
// heap so collections happen.
//
// stdout: one deterministic line; must equal HotSpot's (`java -XX:+UseZGC`).
// stderr: `cpu_ms=` of the whole run (process CPU when available).
//
// Usage: Zt2CaArraySizes [rounds=3000]
public final class Zt2CaArraySizes {
    static final Object[] RING = new Object[256];
    static final int SMALL = 64;

    static int[] lens(int elemSize) {
        int cap = (32768 - 16) / elemSize;
        int[] v = new int[51];
        for (int i = 0; i < 48; i++) {
            v[i] = i;
        }
        v[48] = cap - 1;
        v[49] = cap;
        v[50] = cap + 1;
        return v;
    }

    static final int[] L1 = lens(1);
    static final int[] L2 = lens(2);
    static final int[] L4 = lens(4);
    static final int[] L8 = lens(8);
    static int ringAt;

    static void keep(Object o) {
        RING[ringAt & 255] = o;
        ringAt++;
    }

    static long burstByte(int r) {
        long bad = 0;
        byte[] prev = null;
        for (int k = 0; k < 64; k++) {
            int len = L1[(r * 7 + k) % L1.length];
            byte[] a = new byte[len];
            keep(a);
            if (a.length != len) {
                bad++;
            }
            if (prev != null && prev.length > 0 && prev[prev.length - 1] != (byte) 0x5A) {
                bad++;
            }
            int hi = len <= SMALL ? len : 1;
            for (int i = 0; i < hi; i++) {
                if (a[i] != 0) {
                    bad++;
                }
                a[i] = (byte) 0x5A;
            }
            if (len > 0) {
                if (a[len - 1] != 0 && len > SMALL) {
                    bad++;
                }
                a[len - 1] = (byte) 0x5A;
            }
            prev = a;
        }
        return bad;
    }

    static long burstBoolean(int r) {
        long bad = 0;
        boolean[] prev = null;
        for (int k = 0; k < 64; k++) {
            int len = L1[(r * 5 + k) % L1.length];
            boolean[] a = new boolean[len];
            keep(a);
            if (a.length != len) {
                bad++;
            }
            if (prev != null && prev.length > 0 && !prev[prev.length - 1]) {
                bad++;
            }
            int hi = len <= SMALL ? len : 1;
            for (int i = 0; i < hi; i++) {
                if (a[i]) {
                    bad++;
                }
                a[i] = true;
            }
            if (len > 0) {
                if (a[len - 1] && len > SMALL) {
                    bad++;
                }
                a[len - 1] = true;
            }
            prev = a;
        }
        return bad;
    }

    static long burstChar(int r) {
        long bad = 0;
        char[] prev = null;
        for (int k = 0; k < 64; k++) {
            int len = L2[(r * 3 + k) % L2.length];
            char[] a = new char[len];
            keep(a);
            if (a.length != len) {
                bad++;
            }
            if (prev != null && prev.length > 0 && prev[prev.length - 1] != (char) 0xA55A) {
                bad++;
            }
            int hi = len <= SMALL ? len : 1;
            for (int i = 0; i < hi; i++) {
                if (a[i] != 0) {
                    bad++;
                }
                a[i] = (char) 0xA55A;
            }
            if (len > 0) {
                if (a[len - 1] != 0 && len > SMALL) {
                    bad++;
                }
                a[len - 1] = (char) 0xA55A;
            }
            prev = a;
        }
        return bad;
    }

    static long burstShort(int r) {
        long bad = 0;
        short[] prev = null;
        for (int k = 0; k < 64; k++) {
            int len = L2[(r * 11 + k) % L2.length];
            short[] a = new short[len];
            keep(a);
            if (a.length != len) {
                bad++;
            }
            if (prev != null && prev.length > 0 && prev[prev.length - 1] != (short) 0x5AA5) {
                bad++;
            }
            int hi = len <= SMALL ? len : 1;
            for (int i = 0; i < hi; i++) {
                if (a[i] != 0) {
                    bad++;
                }
                a[i] = (short) 0x5AA5;
            }
            if (len > 0) {
                if (a[len - 1] != 0 && len > SMALL) {
                    bad++;
                }
                a[len - 1] = (short) 0x5AA5;
            }
            prev = a;
        }
        return bad;
    }

    static long burstInt(int r) {
        long bad = 0;
        int[] prev = null;
        for (int k = 0; k < 64; k++) {
            int len = L4[(r * 13 + k) % L4.length];
            int[] a = new int[len];
            keep(a);
            if (a.length != len) {
                bad++;
            }
            if (prev != null && prev.length > 0 && prev[prev.length - 1] != 0x5A5A5A5A) {
                bad++;
            }
            int hi = len <= SMALL ? len : 1;
            for (int i = 0; i < hi; i++) {
                if (a[i] != 0) {
                    bad++;
                }
                a[i] = 0x5A5A5A5A;
            }
            if (len > 0) {
                if (a[len - 1] != 0 && len > SMALL) {
                    bad++;
                }
                a[len - 1] = 0x5A5A5A5A;
            }
            prev = a;
        }
        return bad;
    }

    static long burstFloat(int r) {
        long bad = 0;
        float[] prev = null;
        for (int k = 0; k < 64; k++) {
            int len = L4[(r * 17 + k) % L4.length];
            float[] a = new float[len];
            keep(a);
            if (a.length != len) {
                bad++;
            }
            if (prev != null && prev.length > 0 && prev[prev.length - 1] != 1.5f) {
                bad++;
            }
            int hi = len <= SMALL ? len : 1;
            for (int i = 0; i < hi; i++) {
                if (a[i] != 0.0f) {
                    bad++;
                }
                a[i] = 1.5f;
            }
            if (len > 0) {
                if (a[len - 1] != 0.0f && len > SMALL) {
                    bad++;
                }
                a[len - 1] = 1.5f;
            }
            prev = a;
        }
        return bad;
    }

    static long burstLong(int r) {
        long bad = 0;
        long[] prev = null;
        for (int k = 0; k < 64; k++) {
            int len = L8[(r * 19 + k) % L8.length];
            long[] a = new long[len];
            keep(a);
            if (a.length != len) {
                bad++;
            }
            if (prev != null && prev.length > 0 && prev[prev.length - 1] != 0x5A5A5A5A5A5A5A5AL) {
                bad++;
            }
            int hi = len <= SMALL ? len : 1;
            for (int i = 0; i < hi; i++) {
                if (a[i] != 0) {
                    bad++;
                }
                a[i] = 0x5A5A5A5A5A5A5A5AL;
            }
            if (len > 0) {
                if (a[len - 1] != 0 && len > SMALL) {
                    bad++;
                }
                a[len - 1] = 0x5A5A5A5A5A5A5A5AL;
            }
            prev = a;
        }
        return bad;
    }

    static long burstDouble(int r) {
        long bad = 0;
        double[] prev = null;
        for (int k = 0; k < 64; k++) {
            int len = L8[(r * 23 + k) % L8.length];
            double[] a = new double[len];
            keep(a);
            if (a.length != len) {
                bad++;
            }
            if (prev != null && prev.length > 0 && prev[prev.length - 1] != 2.5) {
                bad++;
            }
            int hi = len <= SMALL ? len : 1;
            for (int i = 0; i < hi; i++) {
                if (a[i] != 0.0) {
                    bad++;
                }
                a[i] = 2.5;
            }
            if (len > 0) {
                if (a[len - 1] != 0.0 && len > SMALL) {
                    bad++;
                }
                a[len - 1] = 2.5;
            }
            prev = a;
        }
        return bad;
    }

    static long burstRef(int r) {
        long bad = 0;
        Object[] prev = null;
        for (int k = 0; k < 64; k++) {
            int len = L8[(r * 29 + k) % L8.length];
            Object[] a = new Object[len];
            keep(a);
            if (a.length != len) {
                bad++;
            }
            if (prev != null && prev.length > 0 && prev[prev.length - 1] != RING) {
                bad++;
            }
            int hi = len <= SMALL ? len : 1;
            for (int i = 0; i < hi; i++) {
                if (a[i] != null) {
                    bad++;
                }
                a[i] = RING;
            }
            if (len > 0) {
                if (a[len - 1] != null && len > SMALL) {
                    bad++;
                }
                a[len - 1] = RING;
            }
            prev = a;
        }
        return bad;
    }

    static long[] negative(int n) {
        long[] last = null;
        for (int i = 0; i < 4; i++) {
            last = new long[n];
        }
        return last;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        long t0 = cpuNanos();
        long bad = 0;
        long allocs = 0;
        for (int r = 0; r < rounds; r++) {
            bad += burstByte(r);
            bad += burstBoolean(r);
            bad += burstChar(r);
            bad += burstShort(r);
            bad += burstInt(r);
            bad += burstFloat(r);
            bad += burstLong(r);
            bad += burstDouble(r);
            bad += burstRef(r);
            allocs += 9 * 64;
        }
        // Warm the negative-length site on positive lengths first, so the
        // throwing call runs the compiled body's screen.
        long warm = 0;
        for (int r = 0; r < rounds; r++) {
            warm += negative(r & 15).length;
        }
        int nase = 0;
        int msgBad = 0;
        for (int r = 0; r < 64; r++) {
            int n = -1 - (r % 5);
            try {
                warm += negative(n).length;
            } catch (NegativeArraySizeException e) {
                nase++;
                if (!String.valueOf(n).equals(e.getMessage())) {
                    msgBad++;
                }
            }
        }
        long t1 = cpuNanos();
        System.out.println(
                "Zt2CaArraySizes check types=9 rounds=" + rounds + " allocs=" + allocs + " bad=" + bad
                        + " warm=" + warm + " nase=" + nase + " nase_msg_bad=" + msgBad + " ok="
                        + (bad == 0 && nase == 64 && msgBad == 0));
        System.err.println("Zt2CaArraySizes cpu_ms=" + ((t1 - t0) / 1_000_000));
    }

    static long cpuNanos() {
        try {
            Object os = java.lang.management.ManagementFactory.getOperatingSystemMXBean();
            if (os instanceof com.sun.management.OperatingSystemMXBean b) {
                long v = b.getProcessCpuTime();
                if (v >= 0) {
                    return v;
                }
            }
        } catch (Throwable t) {
            // fall through to the wall clock
        }
        return System.nanoTime();
    }
}
