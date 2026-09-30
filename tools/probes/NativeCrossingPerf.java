// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Prices the native crossings that a heap ByteBuffer accessor makes under --jdk-only, one per loop, so the
// per-call cost of each can be read off directly. See
// docs/internal/jdk-only/heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918.md (retired)
// Run under the default mode and under --jdk-only; compare ns/call.
import java.lang.ref.Reference;
import java.nio.ByteBuffer;
import java.util.Objects;

public class NativeCrossingPerf {
    static long sink;

    interface Body { void run(int n); }

    static void time(String name, int n, Body b) {
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) {
            long t = System.nanoTime();
            b.run(n);
            best = Math.min(best, System.nanoTime() - t);
        }
        System.out.printf("%-34s %8.1f ns/call%n", name, best / (double) n);
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200_000;
        Object o = new Object();
        ByteBuffer hb = ByteBuffer.allocate(1 << 16);
        ByteBuffer ro = hb.asReadOnlyBuffer();
        time("empty loop", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += i; sink += s; });
        time("Reference.reachabilityFence", n, k -> { for (int i = 0; i < k; i++) Reference.reachabilityFence(o); });
        time("Objects.checkIndex", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += Objects.checkIndex(i & 1023, 1024); sink += s; });
        time("heap get(int)   [checkIndex only]", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += hb.get(i & 1023); sink += s; });
        time("heap getShort(int)", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += hb.getShort((i & 1023) * 2); sink += s; });
        time("heap getInt(int)", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += hb.getInt((i & 1023) * 4); sink += s; });
        time("heap putInt(int,int)", n, k -> { for (int i = 0; i < k; i++) hb.putInt((i & 1023) * 4, i); });
        time("heap getLong(int)", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += hb.getLong((i & 1023) * 8); sink += s; });
        time("readonly heap getInt(int)", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += ro.getInt((i & 1023) * 4); sink += s; });
        System.out.println("sink=" + sink);
    }
}
