// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.HashMap;
import java.util.Map;

/**
 * `Collectors.toMap`'s accumulator is `uniqKeysMapAccumulator`, which calls
 * `map.putIfAbsent(k, v)` per element — not `put` and not `merge`. This
 * prices `putIfAbsent` against `put` on the same keys, at two sizes, so a
 * quadratic stage shows up as a ~4x cost for a 2x size.
 */
public class PutIfAbsentProbe {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 8000;
        String[] tok = new String[n];
        for (int i = 0; i < n; i++) tok[i] = "tok" + i;

        long t0 = System.nanoTime();
        Map<String, Integer> a = new HashMap<>();
        for (int i = 0; i < n; i++) a.put(tok[i], i);
        long tPut = System.nanoTime() - t0;

        t0 = System.nanoTime();
        Map<String, Integer> b = new HashMap<>();
        for (int i = 0; i < n; i++) b.putIfAbsent(tok[i], i);
        long tPIA = System.nanoTime() - t0;

        t0 = System.nanoTime();
        Map<String, Integer> c = new HashMap<>();
        for (int i = 0; i < n; i++) c.computeIfAbsent(tok[i], k -> 0);
        long tCIA = System.nanoTime() - t0;

        t0 = System.nanoTime();
        Map<String, Integer> d = new HashMap<>();
        for (int i = 0; i < n; i++) { d.get(tok[i]); d.put(tok[i], i); }
        long tGetPut = System.nanoTime() - t0;

        System.out.println("PUTIFABSENT n=" + n
                + " put_ms=" + (tPut / 1e6)
                + " putifabsent_ms=" + (tPIA / 1e6)
                + " computeifabsent_ms=" + (tCIA / 1e6)
                + " get_then_put_ms=" + (tGetPut / 1e6)
                + " sizes=" + a.size() + "/" + b.size() + "/" + c.size() + "/" + d.size());
    }
}
