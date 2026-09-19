// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Prices Unsafe / ScopedMemoryAccess crossings under --jdk-only. Needs the internal exports:
//   javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED UnsafeCrossingPerf.java
//   cratonvm [--jdk-only] --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp . UnsafeCrossingPerf
// See docs/jdk-only/heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918.md
import jdk.internal.misc.ScopedMemoryAccess;
import jdk.internal.misc.Unsafe;

public class UnsafeCrossingPerf {
    static long sink;
    interface Body { void run(int n); }

    static void time(String name, int n, Body b) {
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) {
            long t = System.nanoTime();
            b.run(n);
            best = Math.min(best, System.nanoTime() - t);
        }
        System.out.printf("%-38s %8.1f ns/call%n", name, best / (double) n);
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200_000;
        Unsafe u = Unsafe.getUnsafe();
        ScopedMemoryAccess sma = ScopedMemoryAccess.getScopedMemoryAccess();
        byte[] arr = new byte[4096];
        long base = u.arrayBaseOffset(byte[].class);
        time("Unsafe.getInt(arr, off)", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += u.getInt(arr, base + ((i & 511) << 2)); sink += s; });
        time("Unsafe.getIntUnaligned(arr,off,be)", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += u.getIntUnaligned(arr, base + ((i & 511) << 2), true); sink += s; });
        time("Unsafe.putIntUnaligned(arr,off,v,be)", n, k -> { for (int i = 0; i < k; i++) u.putIntUnaligned(arr, base + ((i & 511) << 2), i, true); });
        time("Unsafe.getByte(arr, off)", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += u.getByte(arr, base + (i & 4095)); sink += s; });
        time("SMA.getIntUnaligned(null,arr,off,be)", n, k -> { long s = 0; for (int i = 0; i < k; i++) s += sma.getIntUnaligned(null, arr, base + ((i & 511) << 2), true); sink += s; });
        time("SMA.putIntUnaligned(null,arr,off,v,be)", n, k -> { for (int i = 0; i < k; i++) sma.putIntUnaligned(null, arr, base + ((i & 511) << 2), i, true); });
        System.out.println("sink=" + sink);
    }
}
