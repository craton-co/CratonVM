// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z6/tp (2026-10-13): trigger pacing near a full heap, without the proxy.
//
// Fills the heap with a retained list of int[keepInts] while allocating
// `garbagePerKeep` short-lived Object[4] per retained array (ProxyBoxingOomeProbe's
// ~1:4 retained-to-garbage shape when run with the defaults), until an
// OutOfMemoryError. Then drops everything and checks the heap works again.
//
// stdout (deterministic, compare with HotSpot):
//   zr6tp near-full-fill oome=true recovered=true
// stderr (the measurement; never compared):
//   zr6tp fill retained=<n> beans_before=<name=count,...> beans_after=<...> cpu_ms=<this thread's CPU>
// Count collections from -Xlog:gc the way the ProxyBoxing row does.
//
// Run: cratonvm -Xmx256m -Xlog:gc -cp tools/probes Zr6TpNearFullFill [keepInts=64] [garbagePerKeep=8]
//      java -XX:+UseZGC -Xmx256m -Xlog:gc -cp tools/probes Zr6TpNearFullFill
// Loop counters are `int` stepping by 1 on purpose, as elsewhere in this kit.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;

public class Zr6TpNearFullFill {
    static Object sink;

    /** Per-bean collection counts, "name=count" joined by commas. */
    static String collections() {
        StringBuilder sb = new StringBuilder();
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (sb.length() > 0) {
                sb.append(',');
            }
            sb.append(b.getName().replace(' ', '_')).append('=').append(b.getCollectionCount());
        }
        return sb.toString();
    }

    public static void main(String[] args) {
        int keepInts = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        int garbagePerKeep = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        String gc0 = collections();
        long cpu0 = ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime();
        List<int[]> keep = new ArrayList<>();
        boolean oome = false;
        int retained = 0;
        try {
            for (int r = 0; r < 100_000_000; r++) {
                keep.add(new int[keepInts]);
                for (int i = 0; i < garbagePerKeep; i++) {
                    Object[] g = new Object[4];
                    g[0] = Integer.valueOf(r + i + 1000);
                    sink = g;
                }
            }
        } catch (OutOfMemoryError e) {
            oome = true;
            retained = keep.size();
        }
        keep = null;
        sink = null;
        String gc1 = collections();
        long cpu1 = ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime();
        boolean recovered;
        try {
            List<int[]> again = new ArrayList<>();
            for (int i = 0; i < 10_000; i++) {
                again.add(new int[keepInts]);
            }
            recovered = again.size() == 10_000;
        } catch (OutOfMemoryError e) {
            recovered = false;
        }
        System.err.println("zr6tp fill retained=" + retained + " beans_before=" + gc0 + " beans_after=" + gc1
                + " cpu_ms=" + (cpu1 - cpu0) / 1_000_000);
        System.out.println("zr6tp near-full-fill oome=" + oome + " recovered=" + recovered);
    }
}
