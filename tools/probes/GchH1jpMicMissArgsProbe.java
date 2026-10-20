// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gch h1/jp (2026-10-08): reference arguments through the compiled MIC door's
 * MISS arm under moving collections. One call site ({@code dispatch}) meets
 * ten receiver classes, so its inline cache goes megamorphic, and each class's
 * first compiled call goes through the miss arm, which enters the body its
 * compile probe just answered ({@code call_body_the_mic_probe_just_found},
 * {@code vm/src/jit/helpers.rs}) with two reference arguments; the later calls
 * soak the hit, PIC and megamorphic-table arms under the same stress. A smoke
 * test of the doors' argument words, not a forced re-run. Every callee
 * allocates (so a collection can run inside it), reads both arguments after
 * the allocation, and some throw and are caught by the caller, so a wrong or
 * stale argument word shows up as a wrong checksum, an NPE the program never
 * had, or a crash.
 *
 * <p>STDOUT (deterministic, identical on HotSpot 25):
 * <pre>
 *   micmiss rounds=R calls=C caught=K checksum=X
 * </pre>
 *
 * <pre>
 *   GchH1jpMicMissArgsProbe [rounds=400]
 * </pre>
 * Run it under {@code CRATONVM_DBG=gc-stress=...} for the moving window.
 */
public final class GchH1jpMicMissArgsProbe {
    static final class Box {
        final long v;
        final long[] payload;

        Box(long v) {
            this.v = v;
            this.payload = new long[] {v, v * 3, v * 7};
        }
    }

    abstract static class Op {
        abstract long apply(Box a, long[] data, int i);

        /** Allocates (a collection can run here), then reads both arguments. */
        static long touch(Box a, long[] data, int i, int k) {
            long[] garbage = new long[64 + k];
            garbage[0] = i;
            sinkArr = garbage;
            return a.payload[k % 3] + data[(i + k) % data.length] + a.v * k;
        }
    }

    static Object sinkArr;

    static final class O0 extends Op { long apply(Box a, long[] d, int i) { return touch(a, d, i, 0); } }
    static final class O1 extends Op { long apply(Box a, long[] d, int i) { return touch(a, d, i, 1); } }
    static final class O2 extends Op { long apply(Box a, long[] d, int i) { return touch(a, d, i, 2) ^ 5; } }
    static final class O3 extends Op {
        long apply(Box a, long[] d, int i) {
            long r = touch(a, d, i, 3);
            if ((i & 7) == 3) {
                throw new IllegalStateException("o3");
            }
            return r;
        }
    }
    static final class O4 extends Op { long apply(Box a, long[] d, int i) { return touch(a, d, i, 4) + 11; } }
    static final class O5 extends Op {
        long apply(Box a, long[] d, int i) {
            long r = touch(a, d, i, 5);
            return r / ((i % 5) == 0 ? 1 : (i % 5));
        }
    }
    static final class O6 extends Op { long apply(Box a, long[] d, int i) { return touch(a, d, i, 6) * 3; } }
    static final class O7 extends Op {
        long apply(Box a, long[] d, int i) {
            try {
                return touch(a, d, i, 7) / (i & 1);
            } catch (ArithmeticException e) {
                return a.v + d[0];
            }
        }
    }
    static final class O8 extends Op { long apply(Box a, long[] d, int i) { return touch(a, d, i, 8) - 1; } }
    static final class O9 extends Op { long apply(Box a, long[] d, int i) { return touch(a, d, i, 9) | 1; } }

    static Op make(int k) {
        switch (k) {
            case 0: return new O0();
            case 1: return new O1();
            case 2: return new O2();
            case 3: return new O3();
            case 4: return new O4();
            case 5: return new O5();
            case 6: return new O6();
            case 7: return new O7();
            case 8: return new O8();
            default: return new O9();
        }
    }

    static int caught;

    /** The one megamorphic site. */
    static long dispatch(Op op, Box a, long[] data, int i) {
        try {
            return op.apply(a, data, i);
        } catch (IllegalStateException e) {
            caught++;
            return a.v;
        }
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        long checksum = 0;
        long calls = 0;
        for (int r = 0; r < rounds; r++) {
            long[] data = new long[17];
            for (int j = 0; j < data.length; j++) {
                data[j] = (long) r * 31 + j;
            }
            for (int i = 0; i < 50; i++) {
                // A fresh receiver and fresh young arguments on every call.
                Op op = make((i * 7 + r) % 10);
                Box a = new Box((long) r * 1000 + i);
                checksum = checksum * 31 + dispatch(op, a, data, i);
                calls++;
            }
        }
        System.out.println("micmiss rounds=" + rounds + " calls=" + calls + " caught=" + caught
                + " checksum=" + checksum);
    }
}
