// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/**
 * Prices minting a `MemorySegment`, and reading one byte back.
 *
 * It exists as an A/B control. On 2026-08-22 the class CratonVM stamps onto
 * the segments it mints changed from the `java.lang.foreign.MemorySegment`
 * INTERFACE to a fabricated `cratonvm/internal/foreign/MemorySegmentImpl`,
 * and `try_alloc_concurrent_synthetic` resolves a fabricated name by a
 * different route than a real loaded one. If that route re-probes the
 * classpath per allocation, every FFM allocation in the VM pays for it -- and
 * `AbstractVector.defaultReinterpret` mints one `ofArray` segment per
 * `reinterpretAsInts()`, so the Vector API would pay it per lane group.
 *
 * Both arms are runnable on a binary from BEFORE that change, which the
 * Vector API benchmarks are not (they die on the `checkcast` the change
 * fixes). That is the whole point: this is the one FFM shape whose cost can
 * be compared across the change.
 *
 *   SegmentAllocBench [iterations]
 */
public class SegmentAllocBench {

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;

        long checksum = 0;
        long bestHeap = Long.MAX_VALUE;
        long bestArena = Long.MAX_VALUE;
        long bestSlice = Long.MAX_VALUE;

        for (int pass = 0; pass < 3; pass++) {
            // ofArray: the shape `AbstractVector.defaultReinterpret` uses.
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                MemorySegment s = MemorySegment.ofArray(new byte[32]);
                checksum += s.get(ValueLayout.JAVA_BYTE, 0);
            }
            long dt = System.nanoTime() - t0;
            if (dt < bestHeap) {
                bestHeap = dt;
            }

            // Arena.allocate: the off-heap shape.
            try (Arena arena = Arena.ofConfined()) {
                t0 = System.nanoTime();
                for (int i = 0; i < n; i++) {
                    MemorySegment s = arena.allocate(32, 8);
                    checksum += s.get(ValueLayout.JAVA_BYTE, 0);
                }
                dt = System.nanoTime() - t0;
                if (dt < bestArena) {
                    bestArena = dt;
                }
            }

            // asSlice: mints a segment from a segment, no allocator involved.
            try (Arena arena = Arena.ofConfined()) {
                MemorySegment base = arena.allocate(1024, 8);
                t0 = System.nanoTime();
                for (int i = 0; i < n; i++) {
                    MemorySegment s = base.asSlice(i % 512, 32);
                    checksum += s.get(ValueLayout.JAVA_BYTE, 0);
                }
                dt = System.nanoTime() - t0;
                if (dt < bestSlice) {
                    bestSlice = dt;
                }
            }
        }

        System.out.println("SEGALLOC n=" + n
                + " ofArray_ns=" + (bestHeap / (double) n)
                + " arena_ns=" + (bestArena / (double) n)
                + " asSlice_ns=" + (bestSlice / (double) n)
                + " checksum=" + checksum);
    }
}
