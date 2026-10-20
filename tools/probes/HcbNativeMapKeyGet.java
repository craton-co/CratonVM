// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// hcb (JIT round 16): a user-key `get` on the `java.util` maps that stay
// native in `--compatible` after the `jdk-hash-collections` row
// (`ConcurrentHashMap`, `Hashtable`), plus `HashMap` for the row-off arm.
// The A/B lever is `CRATONVM_COMPAT_MAP_ENUM_KEY_FACTS` (the per-call enum
// key classification, `native-collections` `enum_key_identity`).
//
//   cratonvm --compatible --cp . HcbNativeMapKeyGet [iters]

import java.util.*;
import java.util.concurrent.ConcurrentHashMap;

public class HcbNativeMapKeyGet {
    static final class Key {
        final int k;
        Key(int k) { this.k = k; }
        @Override public int hashCode() { return k * 0x9E3779B1; }
        @Override public boolean equals(Object o) { return o instanceof Key x && x.k == k; }
    }

    static long loop(Map<Key, Integer> m, Key[] ks, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += m.get(ks[i & 63]);
        return s;
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200_000;
        Key[] ks = new Key[64];
        Map<Key, Integer> chm = new ConcurrentHashMap<>();
        Map<Key, Integer> ht = new Hashtable<>();
        Map<Key, Integer> hm = new HashMap<>();
        for (int i = 0; i < 64; i++) {
            ks[i] = new Key(i);
            chm.put(ks[i], i);
            ht.put(ks[i], i);
            hm.put(ks[i], i);
        }
        String[] names = {"chm", "hashtable", "hashmap"};
        List<Map<Key, Integer>> maps = List.of(chm, ht, hm);
        long check = 0;
        for (int w = 0; w < 5; w++) for (Map<Key, Integer> m : maps) check += loop(m, ks, n / 10);
        for (int rep = 0; rep < 3; rep++) {
            for (int r = 0; r < 3; r++) {
                long t0 = System.nanoTime();
                check += loop(maps.get(r), ks, n);
                long dt = System.nanoTime() - t0;
                if (rep == 2) System.out.printf("%s-keyget %.1f ns/op%n", names[r], dt / (double) n);
            }
        }
        System.out.println("checksum=" + check);
    }
}
