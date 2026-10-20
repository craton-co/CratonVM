// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// End-to-end smoke for `-Xms` on all three backends: the flag must be accepted,
// produce no diagnostic, and leave `-Xmx` (Runtime.maxMemory) alone. (Since
// gen r5w2/obs6 the Generational backend reports `maxMemory` as HotSpot Serial
// does, `-Xmx` less the young copy reserve; G1 and ZGC still report `-Xmx`.)
//
// Until 2026-09-20 `-Xms` was accepted and dropped in silence by the two
// non-G1 collectors, including the default one; until 2026-09-21 it was still
// dropped, merely with a warning. See
// docs/internal/gc/heap-xms-xmx-mean-different-things-per-backend-20260920-RETIRED-20260921.md.
//
//   for gc in -XX:+UseZGC -XX:+UseG1GC -XX:+UseGenerationalGC; do
//     ./target/release/cratonvm --java-home <jdk> $gc -Xms512m -Xmx2g \
//         -cp tools/probes XmsProbe
//   done
//
// Runtime.totalMemory reports committed bytes. With ZGC, -Xms64m -Xmx512m
// reports total=64m for this small live set, in both runtime policies. For
// floor enforcement with actual decommit, also run ZgcUncommitFloorProbe
// with -XX:ZUncommitDelay=0 and CRATONVM_ZGC_READABLE_GIVE_BACK=0.
public class XmsProbe {
    static Object[] keep;

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 24;
        keep = new Object[20000];
        for (int i = 0; i < keep.length; i++) {
            keep[i] = new long[4];
        }
        long sink = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 40000; i++) {
                long[] junk = new long[8];
                junk[0] = i;
                sink += junk[0];
            }
            System.gc();
        }
        Runtime rt = Runtime.getRuntime();
        System.out.println("XmsProbe ok sink=" + sink
            + " total=" + (rt.totalMemory() >> 20) + "m"
            + " free=" + (rt.freeMemory() >> 20) + "m"
            + " max=" + (rt.maxMemory() >> 20) + "m");
    }
}
