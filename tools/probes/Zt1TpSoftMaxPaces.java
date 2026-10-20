// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t1/tp (2026-10-19): how far above -XX:SoftMaxHeapSize the heap fills
// between collections, for CRATONVM_ZGC_SOFT_MAX_PACES (opt-in).
//
// A 32 MiB live ring of byte[8192] (every `new` replaces a slot) and 3 GiB of
// allocation through it, paced in 64 MiB bursts. Before each burst slice the
// probe reads heap used (Runtime total - free) and keeps the highest reading
// after the first five collections (warm-up out of the way).
//
// HotSpot's ZGC reads the soft maximum in every director rule, so with
// -Xmx1g -XX:SoftMaxHeapSize=256m its heap stays near 256 MiB. This VM's
// default arm reads it only in the warm-up clause and fills to ~75% of -Xmx
// (768 MiB) between collections; with the switch the live-bytes clause fires
// at 75% of the soft maximum, and the re-arm floor (live + a quarter of the
// free capacity, ~280 MiB here) paces it.
//
// stdout (deterministic, compare with HotSpot):
//   zt1tp soft-max-paces peak=<near|far> ok
//   (near: the peak stayed under 2x the soft maximum, i.e. 512 MiB)
// stderr (the measurement; never compared):
//   [probe] zt1tp soft-max peak_mib=<n> soft_mib=<n> gc_count=<n>
//
// Run: cratonvm -Xmx1g -XX:SoftMaxHeapSize=256m -cp tools/probes Zt1TpSoftMaxPaces
//      java -XX:+UseZGC -Xmx1g -XX:SoftMaxHeapSize=256m -Xlog:gc -cp tools/probes Zt1TpSoftMaxPaces
// Loop counters are `int` stepping by 1 on purpose, as elsewhere in this kit.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

public class Zt1TpSoftMaxPaces {
    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, gc.getCollectionCount());
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        long softMiB = args.length > 0 ? Long.parseLong(args[0]) : 256;
        Runtime rt = Runtime.getRuntime();
        byte[][] ring = new byte[32 * 128][];
        for (int i = 0; i < ring.length; i++) {
            ring[i] = new byte[8192];
        }
        long base = gcCount();
        long peak = 0;
        long sink = 0;
        int k = 0;
        // 48 bursts x 64 MiB = 3 GiB; each burst in 16 slices of 4 MiB.
        for (int burst = 0; burst < 48; burst++) {
            for (int slice = 0; slice < 16; slice++) {
                if (gcCount() - base >= 5) {
                    long used = rt.totalMemory() - rt.freeMemory();
                    if (used > peak) {
                        peak = used;
                    }
                }
                for (int i = 0; i < 512; i++) {
                    byte[] b = new byte[8192];
                    b[i] = (byte) burst;
                    ring[k] = b;
                    k++;
                    if (k == ring.length) {
                        k = 0;
                    }
                    sink += b[i];
                }
            }
            Thread.sleep(10);
        }
        long gcs = gcCount() - base;
        long peakMiB = peak >> 20;
        System.err.println("[probe] zt1tp soft-max peak_mib=" + peakMiB + " soft_mib=" + softMiB
                + " gc_count=" + gcs + " sink=" + sink);
        String bucket = peakMiB < 2 * softMiB ? "near" : "far";
        System.out.println("zt1tp soft-max-paces peak=" + bucket + " ok");
    }
}
