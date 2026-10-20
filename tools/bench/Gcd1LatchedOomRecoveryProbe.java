// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gcd d1/b (2026-09-27): after an {@code OutOfMemoryError} the program drops
 * everything; the next COMPILED allocation must collect before it may throw.
 *
 * <p>Each round fills the heap with {@code long[1024]} blocks held by one
 * static list until the first {@code OutOfMemoryError}, drops the list, and
 * then asks a compiled allocator for 24 MB of fresh {@code long[128 * 1024]}
 * arrays. Filling the heap latches the GC-overhead limit (every forced
 * collection frees nothing while the list is live), and the dropped blocks sit
 * in the OLD generation, which a young collection does not reclaim below its
 * occupancy floor. The interpreter's allocation ladder runs a major before it
 * honours the latched limit; the JIT allocation helpers used to throw at once
 * when the program held no {@code SoftReference}
 * ({@code vm/src/jit/helpers.rs}, {@code jit_latched_overhead_limit_throws}),
 * which is the "OutOfMemoryError escapes the {@code println} after a
 * recovery" shape of {@code GenR4W4NativeStringOomProbe},
 * {@code GenR4W6JitOomRootProbe} and {@code GenR4W4HeapFullThrashProbe}.
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench Gcd1LatchedOomRecoveryProbe})
 * prints, in this order:
 * <pre>
 *   PASS recovery-0
 *   PASS recovery-1
 *   PASS recovery-2
 *   PASS all 3
 * </pre>
 * and exits 0; otherwise the summary is {@code FAIL <n> of 3} and the exit
 * code 1. The recovery verdict is decided before anything is printed, so a
 * failing {@code println} cannot turn a PASS into a FAIL.
 *
 * <p>Commands:
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1LatchedOomRecoveryProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
 *   timeout 300 cratonvm $P Gcd1LatchedOomRecoveryProbe
 *   timeout 300 cratonvm $P --nojit Gcd1LatchedOomRecoveryProbe
 *   CRATONVM_GC_OVERHEAD_PROGRESS=0 timeout 300 cratonvm $P Gcd1LatchedOomRecoveryProbe
 * </pre>
 */
public final class Gcd1LatchedOomRecoveryProbe {
    static List<long[]> fill;
    static long sink;

    /** The compiled allocator: warmed in {@code main}, far past the threshold. */
    static long[] alloc(int n) {
        return new long[n];
    }

    /** Fill to the first OOME, drop, then 24 MB through the compiled allocator. */
    static boolean round() {
        fill = new ArrayList<>();
        try {
            while (true) {
                fill.add(alloc(1024));
            }
        } catch (OutOfMemoryError e) {
            fill = null;
        }
        try {
            long s = 0;
            for (int i = 0; i < 24; i++) {
                final long[] chunk = alloc(128 * 1024);
                chunk[i] = i;
                s += chunk[i];
            }
            sink += s;
            return true;
        } catch (OutOfMemoryError again) {
            fill = null;
            return false;
        }
    }

    public static void main(String[] args) {
        for (int i = 0; i < 20_000; i++) {
            sink += alloc(4).length;
        }
        final boolean[] ok = new boolean[3];
        for (int r = 0; r < ok.length; r++) {
            ok[r] = round();
        }
        int failures = 0;
        for (int r = 0; r < ok.length; r++) {
            if (!ok[r]) {
                failures++;
            }
            System.out.println((ok[r] ? "PASS" : "FAIL") + " recovery-" + r);
        }
        if (failures == 0) {
            System.out.println("PASS all 3");
        } else {
            System.out.println("FAIL " + failures + " of 3");
            System.exit(1);
        }
    }
}
