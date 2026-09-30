// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gen r5w3/oldgen7 (2026-09-26): a tenured set between 1/2 and 2/3 of
 * {@code -Xmx} fits, as it does on HotSpot Serial, whose old generation may
 * grow to {@code MaxHeapSize - MaxNewSize} (2/3 of the heap at its default
 * {@code NewRatio=2})
 * ({@code docs/internal/gc/gengc-r4w4-oldgen4-proposal-old-gen-borrows-the-young-budget-DONE-20260928.md}).
 *
 * <p>Retains {@code mib} MiB (default 152, just under the 153.6 MiB that
 * {@code GenR4W4GrowProbe 60} retained when it was measured to fail here and
 * pass on HotSpot) of 256 KiB {@code byte[]} chunks —
 * below every humongous threshold, so they are promoted through the young
 * generation like ordinary survivors — with young garbage in between, then
 * checks every chunk. An absolute size rather than a fraction of
 * {@code maxMemory()}: the two VMs report different {@code maxMemory()} for the
 * same {@code -Xmx} (HotSpot less one survivor, CratonVM less one semi-space),
 * which made {@code GenR4W4GrowProbe}'s percentage mean different tenured sets
 * on each. Nothing but the verdict is printed on the default path.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xms8m -Xmx256m -cp tools/bench GenR5W3OldBorrowProbe 152
 *   CRATONVM_GC_OLD_BORROW_YOUNG=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xms8m -Xmx256m -cp tools/bench GenR5W3OldBorrowProbe 152
 * </pre>
 * HotSpot prints, and CratonVM with the flag must print:
 * <pre>
 *   retained ok
 *   PASS
 * </pre>
 * Negative controls: CratonVM without the flag (old generation fixed at
 * {@code -Xmx / 2} = 128 MiB) prints {@code FAIL: OutOfMemoryError}; so does
 * HotSpot with {@code -XX:NewRatio=1} (old generation 1/2 of the heap). Add
 * {@code -v} as a second argument to print the heap figures.
 */
public final class GenR5W3OldBorrowProbe {
    static final int CHUNK = 256 * 1024;

    public static void main(String[] args) {
        int mib = args.length > 0 ? Integer.parseInt(args[0]) : 152;
        boolean verbose = args.length > 1 && args[1].equals("-v");
        long target = (long) mib << 20;
        List<byte[]> live = new ArrayList<>((int) (target / CHUNK) + 1);
        long retained = 0;
        try {
            while (retained < target) {
                byte[] c = new byte[CHUNK];
                int i = live.size();
                c[0] = (byte) i;
                c[CHUNK - 1] = (byte) (i * 31 + 7);
                live.add(c);
                retained += CHUNK;
                // Short-lived garbage between survivors, so young collections
                // run and tenure the chunks as they would in an application.
                if ((i & 7) == 0) {
                    byte[] g = new byte[CHUNK];
                    g[1] = 1;
                }
            }
        } catch (OutOfMemoryError e) {
            live = null;
            System.out.println("FAIL: OutOfMemoryError");
            System.exit(1);
            return;
        }
        if (verbose) {
            Runtime rt = Runtime.getRuntime();
            System.out.println("info (not deterministic): retained=" + retained + " total="
                    + rt.totalMemory() + " max=" + rt.maxMemory() + " free=" + rt.freeMemory());
        }
        boolean intact = true;
        for (int i = 0; i < live.size(); i++) {
            byte[] c = live.get(i);
            if (c[0] != (byte) i || c[CHUNK - 1] != (byte) (i * 31 + 7)) {
                intact = false;
                break;
            }
        }
        System.out.println(intact ? "retained ok" : "retained FAILED: a chunk was corrupted");
        System.out.println(intact ? "PASS" : "FAIL");
        if (!intact) {
            System.exit(1);
        }
    }
}
