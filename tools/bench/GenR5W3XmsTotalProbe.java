// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r5w3/oldgen7 (2026-09-26): a fresh heap's {@code Runtime.totalMemory()}
 * is about {@code -Xms}, as on HotSpot Serial, not half of it
 * ({@code docs/internal/gc/gengc-r5w2-obs6-xms-startup-commit-is-half-copy-reserve-FIXED-20260928.md}).
 *
 * <p>Reads {@code totalMemory()} first thing in {@code main}, before the program
 * allocates, and prints whether it is at least seven eighths of the {@code -Xms}
 * passed as the argument (in MiB). HotSpot Serial's {@code capacity()} is
 * {@code -Xms} less one survivor space (a thirtieth of it at the default
 * {@code NewRatio=2}, {@code SurvivorRatio=8}); CratonVM's Generational backend
 * counts one semi-space plus the old generation, and splits the {@code -Xms}
 * startup commit evenly across the two semi-spaces unless
 * {@code CRATONVM_GEN_XMS_USABLE_FIRST=1}, so half of {@code -Xms} lands in the
 * copy reserve, which is not usable heap.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xms64m -Xmx512m -cp tools/bench GenR5W3XmsTotalProbe 64
 *   CRATONVM_GEN_XMS_USABLE_FIRST=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xms64m -Xmx512m -cp tools/bench GenR5W3XmsTotalProbe 64
 * </pre>
 * HotSpot prints, and CratonVM with the flag must print:
 * <pre>
 *   total_at_least_7_8_of_xms=true
 *   PASS
 * </pre>
 * CratonVM without the flag (the default) prints {@code
 * total_at_least_7_8_of_xms=false} and {@code FAIL} (it reads 32m). Add
 * {@code -v} as a second argument to print the figure. The number is taken
 * before the first collection: after it, the Generational backend's young
 * uncommit hands the pre-committed semi-space pages back
 * ({@code docs/internal/gc/gengc-r5w3-oldgen7-young-uncommit-ignores-the-xms-floor-FIXED-20260928.md}).
 */
public final class GenR5W3XmsTotalProbe {
    public static void main(String[] args) {
        long total = Runtime.getRuntime().totalMemory();
        long xmsMib = args.length > 0 ? Long.parseLong(args[0]) : 64;
        boolean verbose = args.length > 1 && args[1].equals("-v");
        long xms = xmsMib << 20;
        if (verbose) {
            System.out.println("info (not deterministic): totalMemory=" + total + " xms=" + xms
                    + " maxMemory=" + Runtime.getRuntime().maxMemory());
        }
        boolean ok = total >= xms / 8 * 7;
        System.out.println("total_at_least_7_8_of_xms=" + ok);
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
