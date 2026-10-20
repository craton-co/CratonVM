// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.HashMap;
import java.util.TreeMap;

/**
 * SBR round 2 wave 9 (lane PUREDOORS): the speed rows Sbr2AuditMicro does not carry, for the
 * {@code CRATONVM_PURE_BYTECODE=1} A/B. One row per run so a failing family does not hide the rest:
 * <pre>cratonvm --jdk-only -cp . Sbr2W9PureAb [rounds] [row]</pre>
 */
public class Sbr2W9PureAb {
    static long sink;
    interface Row { long run(int n); }

    public static void main(String[] a) {
        int rounds = a.length > 0 ? Integer.parseInt(a[0]) : 5;
        String want = a.length > 1 ? a[1] : null;
        java.util.LinkedHashMap<String, Row> rows = new java.util.LinkedHashMap<>();
        rows.put("string.format", n -> { long s = 0; for (int i = 0; i < n; i++) s += String.format("%d-%s-%5.2f", i, "x", i * 0.5).length(); return s; });
        rows.put("hashmap", n -> { HashMap<Integer, Integer> m = new HashMap<>(); long s = 0; for (int i = 0; i < n; i++) { m.put(i & 1023, i); s += m.get((i * 7) & 1023) == null ? 0 : 1; } return s + m.size(); });
        rows.put("arraylist", n -> { ArrayList<Integer> l = new ArrayList<>(); long s = 0; for (int i = 0; i < n; i++) { l.add(i); if (l.size() > 1000) { for (int x : l) s += x; l.clear(); } } return s; });
        rows.put("treemap", n -> { TreeMap<Integer, Integer> m = new TreeMap<>(); long s = 0; for (int i = 0; i < n; i++) { m.put((i * 31) & 4095, i); s += m.firstKey(); } return s; });
        rows.put("stringbuilder", n -> { long s = 0; for (int i = 0; i < n; i++) { StringBuilder b = new StringBuilder(); b.append(i).append('x').append("yz"); s += b.toString().hashCode(); } return s; });
        for (var e : rows.entrySet()) {
            String name = e.getKey();
            if (want != null && !want.equals(name)) continue;
            Row r = e.getValue();
            int n = 2000;
            try {
                long t0 = System.nanoTime();
                sink += r.run(n);
                long per = Math.max(1, (System.nanoTime() - t0) / n);
                n = (int) Math.max(200, Math.min(2_000_000, 150_000_000L / per));
                for (int w = 0; w < 4; w++) sink += r.run(n);
                long[] t = new long[rounds];
                for (int k = 0; k < rounds; k++) { long s0 = System.nanoTime(); sink += r.run(n); t[k] = (System.nanoTime() - s0) / n; }
                java.util.Arrays.sort(t);
                System.out.println(name + " " + t[rounds / 2] + " ns/op (min " + t[0] + " max " + t[rounds - 1] + ", n=" + n + ")");
            } catch (Throwable x) {
                System.out.println(name + " ERROR " + x);
            }
        }
        if (sink == 42) System.out.println("x");
    }
}
