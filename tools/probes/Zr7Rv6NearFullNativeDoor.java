// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;

/**
 * zr z7/rv6: near-full ZGC heap, allocation through native doors that do not
 * collect. The live set is grown to an OutOfMemoryError, then {@code drop}
 * 64 KiB chunks are released (free space ~= drop * 64 KiB). The loop churns
 * small garbage with Java {@code new} (which collects on refusal) and calls
 * VM-native allocators between: {@code Thread.getStackTrace()} and a fresh
 * {@code Throwable}'s stack trace. HotSpot ZGC stalls and collects for every
 * allocation, so no door call fails and every trace is non-empty.
 *
 * <p>Question decided: does the near-full floor ({@code
 * CRATONVM_ZGC_NEAR_FULL_PROGRESS}, default 75, whose 1 MiB minimum exceeds
 * the free space below ~1.33 MiB) let the heap run to exhaustion between
 * occupancy collections so that a non-collecting native door refuses where
 * the legacy floor ({@code =0}) collected first?
 *
 * <p>Usage: {@code java -Xmx64m Zr7Rv6NearFullNativeDoor [drop] [iters]}
 * (defaults 12 and 400000). Prints {@code door_oome=<n> empty_traces=<n>
 * traced=<bool> iters=<n>}; HotSpot: {@code door_oome=0 empty_traces=0}.
 */
public class Zr7Rv6NearFullNativeDoor {
    static volatile Object sink;

    public static void main(String[] args) {
        int drop = args.length > 0 ? Integer.parseInt(args[0]) : 12;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 400_000;
        ArrayList<byte[]> live = new ArrayList<>();
        try {
            while (true) {
                live.add(new byte[64 * 1024]);
            }
        } catch (OutOfMemoryError e) {
            // full
        }
        for (int i = 0; i < drop && !live.isEmpty(); i++) {
            live.remove(live.size() - 1);
        }
        int doorOome = 0;
        int empty = 0;
        int minDepth = Integer.MAX_VALUE;
        int done = 0;
        for (int i = 0; i < iters; i++) {
            try {
                sink = new byte[200 + (i & 255)];
            } catch (OutOfMemoryError e) {
                // a Java `new` that the whole ladder refused: not the question
            }
            if ((i & 15) == 0) {
                try {
                    StackTraceElement[] a = Thread.currentThread().getStackTrace();
                    StackTraceElement[] b = new Throwable().getStackTrace();
                    int d = Math.min(a.length, b.length);
                    if (d == 0) {
                        empty++;
                    }
                    minDepth = Math.min(minDepth, d);
                } catch (OutOfMemoryError e) {
                    doorOome++;
                }
            }
            done++;
        }
        System.out.println("door_oome=" + doorOome + " empty_traces=" + empty
                + " traced=" + (minDepth != Integer.MAX_VALUE) + " iters=" + done);
        sink = live;
    }
}
