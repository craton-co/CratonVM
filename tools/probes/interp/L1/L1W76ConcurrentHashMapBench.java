// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L1 -- bench probe for the
// `JdkConcurrentHashMap` row (`CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP`, default
// OFF): with it, over a real JDK `--compatible` runs the JDK 25
// `ConcurrentHashMap` bodies (CAS on `table`, `synchronized` on a bin head,
// `addCount`'s `baseCount` / `CounterCell`s) instead of the `native-collections`
// stand-ins (a native door per call over a segmented Rust-side model).
//
// A/B, one binary, `--compatible` over a real JDK:
//   cratonvm --compatible L1W76ConcurrentHashMapBench                                          (row off, default)
//   CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP=1 cratonvm --compatible L1W76ConcurrentHashMapBench (row on)
// and `cratonvm L1W76ConcurrentHashMapBench` (jdk-only) as the reference the
// row-on arm should equal. Interleave the arms and take medians of the per-run
// minimums (in-JVM timings swing ~3x on a noisy host). Add `--nojit` for the
// interpreter-only picture.
//
// Rows (stdout, min ns/op over REPS=5 rounds, a 1024-entry Integer -> Integer map):
//   get              get(k) through the class (`invokevirtual`), hits only;
//   get-iface        get(k) through `ConcurrentMap` (`invokeinterface`: the JIT's
//                    direct-helper site), hits only;
//   put-overwrite    put(k, v) on a present key;
//   computeIfAbsent  computeIfAbsent(k, f) on a present key (no call of f);
//   merge            merge(k, 1, Integer::sum) on a present key;
//   iterate          one pass of values() over the 1024 entries, ns per element;
//   size             size().
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum get=51031728 get-iface=51031728 put-overwrite=51031728 computeIfAbsent=51031728 merge=1024 iterate=104755200 size=100000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;

public class L1W76ConcurrentHashMapBench {
    static final int REPS = 5;
    static final int SIZE = 1024;
    static final int N = 100_000;
    static final int ITERATES = 200;

    static ConcurrentHashMap<Integer, Integer> base() {
        ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        for (int k = 0; k < SIZE; k++) {
            m.put(k, k);
        }
        return m;
    }

    static long get(ConcurrentHashMap<Integer, Integer> m) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += m.get(i & (SIZE - 1));
        }
        return sum;
    }

    static long getIface(ConcurrentMap<Integer, Integer> m) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += m.get(i & (SIZE - 1));
        }
        return sum;
    }

    static long putOverwrite(ConcurrentHashMap<Integer, Integer> m) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            int k = i & (SIZE - 1);
            Integer old = m.put(k, k);
            sum += old;
        }
        return sum;
    }

    static long computeIfAbsent(ConcurrentHashMap<Integer, Integer> m) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += m.computeIfAbsent(i & (SIZE - 1), k -> -1);
        }
        return sum;
    }

    /** Merges into a separate counter map; its checksum is the key count. */
    static long merge() {
        ConcurrentHashMap<Integer, Integer> c = new ConcurrentHashMap<>();
        for (int i = 0; i < N; i++) {
            c.merge(i & (SIZE - 1), 1, Integer::sum);
        }
        return c.size();
    }

    static long iterate(ConcurrentHashMap<Integer, Integer> m) {
        long sum = 0;
        for (int r = 0; r < ITERATES; r++) {
            for (Integer v : m.values()) {
                sum += v;
            }
        }
        return sum;
    }

    static long size(ConcurrentHashMap<Integer, Integer> m) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += m.size() == SIZE ? 1 : 0;
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        ConcurrentHashMap<Integer, Integer> m = base();
        String[] names = {"get", "get-iface", "put-overwrite", "computeIfAbsent", "merge", "iterate", "size"};
        long[] per = {N, N, N, N, N, (long) ITERATES * SIZE, N};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        for (int r = 0; r < REPS; r++) {
            long[] t = new long[names.length + 1];
            t[0] = System.nanoTime();
            sums[0] = get(m);
            t[1] = System.nanoTime();
            sums[1] = getIface(m);
            t[2] = System.nanoTime();
            sums[2] = putOverwrite(m);
            t[3] = System.nanoTime();
            sums[3] = computeIfAbsent(m);
            t[4] = System.nanoTime();
            sums[4] = merge();
            t[5] = System.nanoTime();
            sums[5] = iterate(m);
            t[6] = System.nanoTime();
            sums[6] = size(m);
            t[7] = System.nanoTime();
            for (int i = 0; i < names.length; i++) {
                best[i] = Math.min(best[i], t[i + 1] - t[i]);
            }
        }
        for (int i = 0; i < names.length; i++) {
            System.out.printf("%-16s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
        }
        System.out.println("checksum get=" + sums[0] + " get-iface=" + sums[1] + " put-overwrite=" + sums[2]
                + " computeIfAbsent=" + sums[3] + " merge=" + sums[4] + " iterate=" + sums[5]
                + " size=" + sums[6]);
        System.out.println("done");
    }
}
