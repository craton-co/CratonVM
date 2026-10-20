// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Drives the ALLOCATION-FAILURE path, which is the only one
// `interpreter::note_gc_productivity` runs on, so that CRATONVM_DBG_GC_OVERHEAD=1
// prints its `promoted=` term for a real workload.
//
// Before 2026-09-21 that term was a hard `0` on G1 and ZGC whatever the workload
// did, because neither collector had a promoted-bytes counter and the `VmHeap`
// dispatcher answered a constant. See
// docs/internal/gc/heap-gc-overhead-limit-reads-two-hard-zeros-on-g1-and-zgc-20260920-RETIRED-20260921.md.
//
//   ./target/release/cratonvm --java-home <jdk> -XX:+UseG1GC -Xmx96m \
//       -cp tools/probes GcOverheadProbe
//
// with CRATONVM_DBG_GC_OVERHEAD=1 in the environment; add
// CRATONVM_ZGC_GENERATIONAL=1 for the ZGC arm, which has no old generation to
// promote into without it (and correctly reports `promoted=0` in that case).
//
// The shape is a retained set that grows without bound plus churn between
// rounds: the heap fills, allocation fails, and every failure forces a
// collection whose productivity is then scored. The OOM at the end is the
// designed outcome, not a failure of the probe.
import java.util.ArrayList;
import java.util.List;

public class GcOverheadProbe {
    public static void main(String[] args) {
        List<Object> keep = new ArrayList<>();
        long sink = 0;
        try {
            for (int r = 0; r < 100000; r++) {
                for (int i = 0; i < 200; i++) {
                    keep.add(new long[32]);
                }
                for (int i = 0; i < 2000; i++) {
                    long[] junk = new long[16];
                    junk[0] = i;
                    sink += junk[0];
                }
            }
        } catch (OutOfMemoryError e) {
            System.out.println("GcOverheadProbe OOM as designed after " + keep.size()
                + " retained, sink=" + sink);
            return;
        }
        System.out.println("GcOverheadProbe finished without OOM, sink=" + sink);
    }
}
