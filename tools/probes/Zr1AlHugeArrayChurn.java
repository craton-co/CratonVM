// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z1/al (2026-10-12): a dead array over the 256 MiB `alloc_size` screen
// was never reclaimed on ZGC.
//
// `ZgcRealHeap::alloc_size` refuses an array whose payload exceeds 256 MiB
// (`MAX_PLAUSIBLE_BODY`) as a corruption screen, while the allocator admits up
// to `i32::MAX` elements. Such an array lands on the large-object end, and the
// large-object sweep (`sweep_one_high`) RETAINED every base it could not size.
// So a loop that allocates one such array and drops it each iteration ran out
// of heap after a handful of iterations, where HotSpot ZGC runs it forever.
// Fixed behind `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS` (default ON, `=0`
// restores the retention).
//
// Usage: java -Xmx1g Zr1AlHugeArrayChurn [mib=300] [iterations=40]
// Pass: `served=<iterations> oom=0 PASS`.
// Read `[GC] zgc-alloc-sweep: ... oversize_arrays_swept=` with
// CRATONVM_GC_STATS=1: it must be >= iterations - 1 on the fixed binary, and 0
// with the kill switch.
public final class Zr1AlHugeArrayChurn {
    public static void main(String[] args) {
        int mib = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        int iterations = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        int len = mib << 20;
        long checksum = 0;
        int served = 0;
        int oom = 0;
        for (int i = 0; i < iterations; i++) {
            try {
                byte[] a = new byte[len];
                a[i % len] = (byte) i;
                a[len - 1] = 1;
                checksum += a[i % len] + a[len - 1] + a.length;
                served++;
            } catch (OutOfMemoryError e) {
                oom++;
                // HotSpot never gets here; keep going so the line still shows
                // how many iterations the heap could serve.
            }
        }
        System.out.println("Zr1AlHugeArrayChurn mib=" + mib + " iterations=" + iterations
                + " served=" + served + " oom=" + oom + " checksum=" + checksum
                + (served == iterations && oom == 0 ? " PASS" : " FAIL"));
    }
}
