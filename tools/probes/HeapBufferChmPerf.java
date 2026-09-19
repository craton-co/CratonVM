// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Two-row ratio probe for docs/jdk-only/heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918.md:
// heap ByteBuffer absolute accessors and ConcurrentHashMap put+get, timed per round so warm-up is visible.
// Run under the default mode and under --jdk-only and compare each round.
import java.nio.ByteBuffer;
import java.util.concurrent.ConcurrentHashMap;

public class HeapBufferChmPerf {
    public static void main(String[] a) {
        int rounds = a.length > 0 ? Integer.parseInt(a[0]) : 4;
        int n = a.length > 1 ? Integer.parseInt(a[1]) : 500_000;
        long s = 0;
        ByteBuffer hb = ByteBuffer.allocate(1 << 16);
        ByteBuffer db = ByteBuffer.allocateDirect(1 << 16);
        for (int r = 0; r < rounds; r++) {
            long t = System.nanoTime();
            for (int i = 0; i < n; i++) {
                hb.putInt((i & 1023) * 4, i);
                s += hb.getInt((i & 1023) * 4);
            }
            long heap = (System.nanoTime() - t) / 1_000_000;

            t = System.nanoTime();
            for (int i = 0; i < n; i++) {
                db.putInt((i & 1023) * 4, i);
                s += db.getInt((i & 1023) * 4);
            }
            long direct = (System.nanoTime() - t) / 1_000_000;

            t = System.nanoTime();
            ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
            for (int i = 0; i < n; i++) m.put(i, i);
            for (int i = 0; i < n; i++) s += m.get(i);
            long chm = (System.nanoTime() - t) / 1_000_000;

            System.out.println("round " + r + ": heapBB=" + heap + "ms directBB=" + direct + "ms chm=" + chm + "ms");
        }
        System.out.println("sink=" + s);
    }
}
