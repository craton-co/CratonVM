// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t1/tp (2026-10-19): the ZGC director's warmth, for
// CRATONVM_ZGC_DIRECTOR_HOTSPOT_WARMTH.
//
// HotSpot's ZStatCycle counts WARM-UP cycles: its allocation-rate rule is off
// until one warm-up cycle has run (is_time_trustable) and its proactive rule
// until three (is_warm). A System.gc() is a cycle but not a warm-up one. The
// director (CRATONVM_ZGC_RATE_TRIGGER=1 / CRATONVM_ZGC_TRIGGER_SHADOW=1)
// counted every cycle before zt.
//
// Shape: three rounds of { System.gc(); ~8 MiB of paced garbage over ~300 ms }
// -- far below the first warm-up point (10% of the soft max) at -Xmx512m, so
// no warm-up cycle runs, and slow enough that the director takes rate samples
// between the System.gc() cycles -- then a 128 MiB/s stream that reaches the
// three warm-up points.
//
// stdout (deterministic, compare with HotSpot):
//   zt1tp director-warmth sysgc=3 ok
// stderr (never compared): under CRATONVM_ZGC_TRIGGER_SHADOW=1, the
// `[GC] zgc-trigger-shadow:` lines of the 2nd and 3rd System.gc() cycles carry
// `hs_time_until_gc_ms=inf` with the switch on (the rate rule is off: no
// warm-up cycle yet) and a finite value at `=0` (the zr rule counted the
// System.gc() cycles).
//
// Run: CRATONVM_ZGC_TRIGGER_SHADOW=1 cratonvm -Xmx512m -cp tools/probes Zt1TpDirectorWarmth
//      java -XX:+UseZGC -Xmx512m -Xlog:gc -cp tools/probes Zt1TpDirectorWarmth
// Loop counters are `int` stepping by 1 on purpose, as elsewhere in this kit.
public class Zt1TpDirectorWarmth {
    static Object sink;

    public static void main(String[] args) throws Exception {
        long checksum = 0;
        for (int round = 0; round < 3; round++) {
            System.gc();
            // 128 x 64 KiB = 8 MiB, in 16 slices of 8 with a sleep between.
            for (int slice = 0; slice < 16; slice++) {
                for (int i = 0; i < 8; i++) {
                    byte[] b = new byte[65536];
                    b[i] = (byte) slice;
                    checksum += b[i];
                    sink = b;
                }
                Thread.sleep(20);
            }
        }
        // The stream: 1024 x 64 KiB = 64 MiB per ~0.5 s, 6 times (384 MiB):
        // past 10, 20 and 30% of a 512 MiB soft max.
        for (int burst = 0; burst < 6; burst++) {
            for (int i = 0; i < 1024; i++) {
                byte[] b = new byte[65536];
                b[i & 1023] = 1;
                checksum += b[i & 1023];
                sink = b;
            }
            Thread.sleep(500);
        }
        System.err.println("[probe] zt1tp director-warmth checksum=" + checksum);
        System.out.println("zt1tp director-warmth sysgc=3 ok");
    }
}
