// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.ArrayList;

/**
 * zs s1/nd: native doors on a near-full heap (ZGC round zs, wave s1, lane nd).
 *
 * <p>The live set is grown with 64 KiB chunks until about {@code freeKb} KiB of
 * the heap is left (measured from {@code Runtime} after a {@code System.gc()},
 * the {@code Zr8NrNativeDoors} / {@code Zr11NdNativeReplay} fill, so HotSpot
 * finishes). The churn loop then makes its garbage through the door under test:
 *
 * <ul>
 *   <li>{@code sbvalue}: {@code String.contentEquals(StringBuilder)} on a
 *       2048-character builder: JDK bytecode reads
 *       {@code AbstractStringBuilder.getValue()}, which under
 *       {@code --compatible} is a native that copies a synthetic builder into
 *       a fresh {@code byte[]} through the one-attempt door; now on the
 *       native funnel's restartable list
 *       ({@code CRATONVM_ZGC_NATIVE_OOM_REPLAY}). Under {@code --jdk-only}
 *       the builder is the JDK's own and nothing is copied (control);
 *   <li>{@code segowner}: reads and writes of a CONFINED arena's segment
 *       from a hot loop (compiled: the JIT's segment helpers check the
 *       session owner; since zs s1/nd they compare against the existing
 *       {@code Thread} mirror instead of minting one), with 512-byte garbage
 *       in between;
 *   <li>{@code mixed}: both, in turn.
 * </ul>
 *
 * <p>HotSpot collects for every refused allocation: {@code door_oome=0} on
 * every mode. Only the last line is printed (stdout, deterministic).
 * Usage: {@code java -Xmx64m Zs1NdNativeDoors <mode> [freeKb] [iters]}
 * (defaults 1024 and 20000).
 */
public class Zs1NdNativeDoors {
    static volatile Object sink;

    static final String TEXT;

    static {
        StringBuilder t = new StringBuilder();
        for (int i = 0; i < 2048; i++) {
            t.append((char) ('a' + i % 26));
        }
        TEXT = t.toString();
    }

    static long segRound(MemorySegment seg, int i) {
        long sum = 0;
        for (int k = 0; k < 64; k++) {
            seg.setAtIndex(ValueLayout.JAVA_INT, k, i + k);
            sum += seg.getAtIndex(ValueLayout.JAVA_INT, k);
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "sbvalue";
        int freeKb = args.length > 1 ? Integer.parseInt(args[1]) : 1024;
        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 20_000;
        StringBuilder sb = new StringBuilder(TEXT);
        long check = 0;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(256, 8);
            // Warm both doors before the heap is full (linkage, compilation).
            for (int i = 0; i < 20_000; i++) {
                if (!TEXT.contentEquals(sb)) {
                    throw new AssertionError("warm contentEquals");
                }
                check += segRound(seg, i) & 1;
            }
            check = 0;
            System.gc();
            Runtime rt = Runtime.getRuntime();
            long used0 = rt.totalMemory() - rt.freeMemory();
            long room = rt.maxMemory() - used0 - (long) freeKb * 1024;
            ArrayList<byte[]> live = new ArrayList<>();
            try {
                for (long n = Math.max(0, room / (64 * 1024)); n > 0; n--) {
                    live.add(new byte[64 * 1024 - 16]);
                }
            } catch (OutOfMemoryError e) {
                for (int i = 0; i < 4 && !live.isEmpty(); i++) {
                    live.remove(live.size() - 1);
                }
            }
            int doorOome = 0;
            int bad = 0;
            int done = 0;
            for (int i = 0; i < iters; i++) {
                String m = mode.equals("mixed") ? ((i & 1) == 0 ? "sbvalue" : "segowner") : mode;
                try {
                    switch (m) {
                        case "sbvalue":
                            if (!TEXT.contentEquals(sb)) {
                                bad++;
                            }
                            break;
                        case "segowner": {
                            long s = segRound(seg, i);
                            // sum of (i + k) for k in 0..63
                            if (s != 64L * i + 2016L) {
                                bad++;
                            }
                            check += s & 0xFF;
                            sink = new byte[512];
                            break;
                        }
                        default:
                            throw new IllegalArgumentException(mode);
                    }
                } catch (OutOfMemoryError e) {
                    doorOome++;
                }
                done++;
            }
            System.out.println("mode=" + mode + " door_oome=" + doorOome + " bad=" + bad
                    + " iters=" + done + " filled=" + !live.isEmpty()
                    + " check=" + (mode.equals("sbvalue") ? 0 : check));
            sink = live;
        }
    }
}
