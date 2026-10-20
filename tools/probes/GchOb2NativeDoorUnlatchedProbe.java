// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.List;

/**
 * gch h2/ob (2026-10-08): a NATIVE array factory refused on an UNLATCHED heap
 * whose young collection alone frees what the request needs -- the case
 * {@code gcg-g1o-proposal-native-factory-door-tries-after-the-young-collection}
 * ({@code CRATONVM_GC_NATIVE_FACTORY_ATTEMPT_LADDER}) is about, which no
 * earlier row reached (vg3 / vg5 met only a latched streak, where the staged
 * door does not apply).
 *
 * <p>Shape: an old-generation live set that leaves less free than one request
 * (its target is the old pool's max minus half a request, then
 * {@code System.gc()} tenures it); then, each round, short-lived garbage until
 * eden is about 70 % used, and {@code Array.newInstance(long.class, n)} for 40 %
 * of eden -- under half the semi-space, so not humongous here, and too large
 * for what is left of either generation. The genuine JDK native
 * {@code Array.newArray} is the refused factory in every mode. A young
 * collection empties eden, after which the request fits; the classic door
 * (switch off) runs that young collection PLUS a last-ditch full collection
 * and a second major before its one retry, the staged door (switch on) tries
 * right after the young one.
 *
 * <p>Run with {@code -Xmx64m} and, on CratonVM,
 * {@code CRATONVM_GC_YOUNG_TRIGGER_PERCENT=95} (the default 50 % trigger
 * collects before eden is 70 % used, so the request would always fit).
 * stdout is deterministic: {@code GchOb2NativeDoorUnlatched rounds=<r> ok=<r>
 * PROBE-OK} (or {@code PROBE-FAIL} with the OOME count). The geometry and the
 * rounds whose churn reached the 70 % mark go to stderr ({@code [probe] ...}).
 * Arg 0: rounds (default 16).
 *
 * <p>Deciding figures (CratonVM stderr, {@code CRATONVM_GC_STATS=1}):
 * {@code [GC] generational: minor= major=} and
 * {@code [GC] oome_ladder_doors: factory_enter= factory_retry=}. Off:
 * {@code major} about {@code 2 * factory_enter + 1}; on: about 1 (the
 * {@code System.gc()}), stdout SAME on both.
 */
public final class GchOb2NativeDoorUnlatchedProbe {
    static volatile Object sink;
    static long checksum;

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 16;
        long max = Runtime.getRuntime().maxMemory();
        MemoryPoolMXBean old = null;
        MemoryPoolMXBean eden = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.HEAP) {
                continue;
            }
            String n = p.getName();
            if (n.contains("Tenured") || n.contains("Old")) {
                old = p;
            } else if (n.contains("Eden")) {
                eden = p;
            }
        }
        long oldMax = old != null && old.getUsage().getMax() > 0 ? old.getUsage().getMax() : max * 2 / 3;
        long edenMax = eden != null && eden.getUsage().getMax() > 0 ? eden.getUsage().getMax() : max / 4;
        int reqLongs = (int) (edenMax * 40 / 100 / 8);
        long reqBytes = (long) reqLongs * 8;

        // The old-generation live set: 64 KiB pieces up to oldMax - req / 2.
        List<long[]> live = new ArrayList<>();
        long target = oldMax - reqBytes / 2;
        long held = 0;
        while (held + 65536 <= target) {
            live.add(new long[8192]);
            held += 65536;
        }
        System.gc();
        System.err.println("[probe] max=" + max + " old_max=" + oldMax + " eden_max=" + edenMax
                + " req_bytes=" + reqBytes + " held=" + held
                + " old_used=" + (old != null ? old.getUsage().getUsed() : -1));

        int ok = 0;
        int oome = 0;
        int reached = 0;
        for (int r = 0; r < rounds; r++) {
            sink = null;
            // Churn until eden is ~70 % used (checked every 256 pieces; bounded
            // at two edens of garbage, in case a collection resets it).
            long churned = 0;
            boolean hit = false;
            while (churned < 2 * edenMax) {
                for (int i = 0; i < 256; i++) {
                    sink = new byte[1024];
                }
                churned += 256 * 1040;
                if (eden == null || eden.getUsage().getUsed() * 100 >= edenMax * 70) {
                    hit = eden != null;
                    break;
                }
            }
            if (hit) {
                reached++;
            }
            sink = null;
            try {
                Object a = Array.newInstance(long.class, reqLongs);
                checksum += Array.getLength(a);
                ok++;
            } catch (OutOfMemoryError e) {
                oome++;
            }
        }
        System.err.println("[probe] rounds=" + rounds + " reached_70pct=" + reached + " oome=" + oome
                + " live=" + live.size() + " checksum=" + checksum);
        if (ok == rounds) {
            System.out.println("GchOb2NativeDoorUnlatched rounds=" + rounds + " ok=" + ok + " PROBE-OK");
        } else {
            System.out.println("GchOb2NativeDoorUnlatched rounds=" + rounds + " ok=" + ok + " oome=" + oome
                    + " PROBE-FAIL");
        }
    }
}
