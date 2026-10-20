// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L5 -- re-measures the `ConcurrentHashMap`
// + `Properties` family under `--compatible`
// (`i52-L5-proposal-a-compatible-row-per-retired-shadow-family`). The wave-91
// triage quoted 27 174 ns per user-key `ConcurrentHashMap.get` from the hcb
// table (JIT round 16), which predates the `jdk-concurrent-hash-map` row
// (interpreter round i3 wave 76 / nio21 wave 1, default ON). This bench prices
// the family with the row on and off, and `Hashtable` (the next candidate
// family, `RETIRED_SHADOW_L1_HT_TRIPLES`, no row yet) beside it.
//
// Rows (ns per op, last of 3 reps; the checksum keeps the loops live):
//   chm-keyget        user-key `ConcurrentHashMap.get`
//   props-getprop     `Properties.getProperty` on a 64-entry `Properties`
//   sysprop-get       `System.getProperty("java.version")`
//   hashtable-keyget  user-key `Hashtable.get` (no row; for the next family)
//
// A/B (orchestrator; interleave the two arms, take medians):
//   cratonvm --compatible -cp out L5W92ChmPropertiesBench
//   CRATONVM_COMPAT_ALIGN=-jdk-concurrent-hash-map cratonvm --compatible -cp out L5W92ChmPropertiesBench
//   cratonvm -cp out L5W92ChmPropertiesBench            (--jdk-only reference)
//   java -cp out L5W92ChmPropertiesBench                (HotSpot reference)

import java.util.Hashtable;
import java.util.Map;
import java.util.Properties;
import java.util.concurrent.ConcurrentHashMap;

public class L5W92ChmPropertiesBench {
    static final class Key {
        final int k;
        Key(int k) { this.k = k; }
        @Override public int hashCode() { return k * 0x9E3779B1; }
        @Override public boolean equals(Object o) { return o instanceof Key x && x.k == k; }
    }

    static long mapLoop(Map<Key, Integer> m, Key[] ks, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += m.get(ks[i & 63]);
        return s;
    }

    static long propsLoop(Properties p, String[] names, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += p.getProperty(names[i & 63]).length();
        return s;
    }

    static long sysLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += System.getProperty("java.version").length();
        return s;
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200_000;
        Key[] ks = new Key[64];
        String[] names = new String[64];
        Map<Key, Integer> chm = new ConcurrentHashMap<>();
        Map<Key, Integer> ht = new Hashtable<>();
        Properties props = new Properties();
        for (int i = 0; i < 64; i++) {
            ks[i] = new Key(i);
            names[i] = "l5w92.prop." + i;
            chm.put(ks[i], i);
            ht.put(ks[i], i);
            props.setProperty(names[i], "v" + i);
        }
        long check = 0;
        for (int w = 0; w < 5; w++) {
            check += mapLoop(chm, ks, n / 10);
            check += propsLoop(props, names, n / 10);
            check += sysLoop(n / 10);
            check += mapLoop(ht, ks, n / 10);
        }
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            check += mapLoop(chm, ks, n);
            long t1 = System.nanoTime();
            check += propsLoop(props, names, n);
            long t2 = System.nanoTime();
            check += sysLoop(n);
            long t3 = System.nanoTime();
            check += mapLoop(ht, ks, n);
            long t4 = System.nanoTime();
            if (rep == 2) {
                System.out.printf("chm-keyget %.1f ns/op%n", (t1 - t0) / (double) n);
                System.out.printf("props-getprop %.1f ns/op%n", (t2 - t1) / (double) n);
                System.out.printf("sysprop-get %.1f ns/op%n", (t3 - t2) / (double) n);
                System.out.printf("hashtable-keyget %.1f ns/op%n", (t4 - t3) / (double) n);
            }
        }
        System.out.println("checksum=" + check);
    }
}
