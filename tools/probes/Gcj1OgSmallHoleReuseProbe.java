// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j1/og (2026-10-19): FIXED-WORK small-object hole reuse in the old
 * generation, the deciding probe of
 * {@code gci-i47w-j41-exact-start-class-thrash-counters-above-the-kill-arm}
 * ({@code CRATONVM_GC_OLD_EXACT_START_CLASS}, default ON, {@code =0} kills).
 *
 * <p>The thrash probe that raised that page allocates until
 * {@code OutOfMemoryError}, so its counters scale with how long each arm ran.
 * This one does the same amount of work on both arms: {@code live} small
 * objects of four sizes (one to four {@code long} fields: 16, 24, 32 and 40
 * bytes with an 8-byte header) are tenured, then {@code steps} times a fixed
 * pseudo-random {@code replace} of them are replaced by fresh objects of the
 * SAME size (so the replaced object leaves an exact-size hole in the old
 * generation and its replacement is promoted), with young churn in between.
 * An exact-fit start class reuses those holes; the {@code size + align - 1}
 * start skips the 16- and 24-byte classes.
 *
 * <p>stdout is identical on HotSpot and CratonVM:
 * <pre>
 *   Gcj1OgSmallHoleReuseProbe live=300000 steps=200 replace=6000
 *   checksum=&lt;n&gt;
 *   PROBE-OK
 * </pre>
 * stderr: {@code [probe] loop-ms=<wall>}. The deciding numbers are the
 * {@code [GC]} summary's {@code oldgen_alloc_failures / oldgen_allocs},
 * {@code oldsz_frag_refusals}, {@code trig_requested} and
 * {@code oldsz_compactions} under {@code CRATONVM_GC_STATS=1}.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -cp tools/probes Gcj1OgSmallHoleReuseProbe
 *   CRATONVM_GC_STATS=1 cratonvm -XX:+UseGenerationalGC -Xmx64m -cp tools/probes Gcj1OgSmallHoleReuseProbe
 * </pre>
 * Args (all optional): live steps replace.
 */
public final class Gcj1OgSmallHoleReuseProbe {
    static class S1 {
        long a;

        S1(long a) {
            this.a = a;
        }

        long sum() {
            return a;
        }

        S1 fresh() {
            return new S1(a);
        }
    }

    static final class S2 extends S1 {
        long b;

        S2(long a) {
            super(a);
            b = a + 1;
        }

        @Override
        long sum() {
            return a + b;
        }

        @Override
        S1 fresh() {
            return new S2(a);
        }
    }

    static final class S3 extends S1 {
        long b;
        long c;

        S3(long a) {
            super(a);
            b = a + 1;
            c = a + 2;
        }

        @Override
        long sum() {
            return a + b + c;
        }

        @Override
        S1 fresh() {
            return new S3(a);
        }
    }

    static final class S4 extends S1 {
        long b;
        long c;
        long d;

        S4(long a) {
            super(a);
            b = a + 1;
            c = a + 2;
            d = a + 3;
        }

        @Override
        long sum() {
            return a + b + c + d;
        }

        @Override
        S1 fresh() {
            return new S4(a);
        }
    }

    static volatile Object sink;

    public static void main(String[] args) {
        int live = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;
        int steps = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        int replace = args.length > 2 ? Integer.parseInt(args[2]) : 6000;
        System.out.println("Gcj1OgSmallHoleReuseProbe live=" + live + " steps=" + steps
                + " replace=" + replace);
        S1[] objs = new S1[live];
        for (int i = 0; i < live; i++) {
            switch (i & 3) {
                case 0 -> objs[i] = new S1(i);
                case 1 -> objs[i] = new S2(i);
                case 2 -> objs[i] = new S3(i);
                default -> objs[i] = new S4(i);
            }
        }
        long rng = 0x2545F4914F6CDD1DL;
        long t0 = System.nanoTime();
        for (int s = 0; s < steps; s++) {
            for (int k = 0; k < replace; k++) {
                rng ^= rng << 13;
                rng ^= rng >>> 7;
                rng ^= rng << 17;
                int i = (int) ((rng >>> 1) % live);
                objs[i] = objs[i].fresh();
            }
            for (int k = 0; k < 256; k++) {
                sink = new byte[8 * 1024];
            }
        }
        long loopMs = (System.nanoTime() - t0) / 1_000_000;
        long checksum = 0;
        for (S1 o : objs) {
            checksum += o.sum();
        }
        System.out.println("checksum=" + checksum);
        System.out.println("PROBE-OK");
        System.err.println("[probe] loop-ms=" + loopMs);
    }
}
