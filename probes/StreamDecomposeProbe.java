// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.HashMap;
import java.util.Map;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

/**
 * Splits `IntStream.range(n).boxed().collect(Collectors.toMap(..))` into its
 * parts, to find which one is quadratic on CratonVM.
 *
 *   traverse  — the stream pipeline alone, no collector
 *   merge     — HashMap.merge in a plain loop (what toMap calls per element)
 *   put       — HashMap.put in a plain loop (the linear control)
 *   toMap     — the whole thing
 *
 * Each is timed over the same keys. Run at two sizes and compare the ratio:
 * a linear stage roughly doubles when n doubles, a quadratic one roughly
 * quadruples.
 */
public class StreamDecomposeProbe {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 8000;
        String[] tok = new String[n];
        for (int i = 0; i < n; i++) tok[i] = "tok" + i;

        long t0 = System.nanoTime();
        long traversed = IntStream.range(0, n).boxed().count();
        long tTraverse = System.nanoTime() - t0;

        t0 = System.nanoTime();
        Map<String, Integer> mp = new HashMap<>();
        for (int i = 0; i < n; i++) mp.put(tok[i], i);
        long tPut = System.nanoTime() - t0;

        t0 = System.nanoTime();
        Map<String, Integer> mm = new HashMap<>();
        for (int i = 0; i < n; i++) {
            final int v = i;
            mm.merge(tok[i], v, (a, b) -> { throw new IllegalStateException(); });
        }
        long tMerge = System.nanoTime() - t0;

        // Stream traversal feeding a plain put, to separate "stream" from
        // "collector".
        t0 = System.nanoTime();
        Map<String, Integer> mf = new HashMap<>();
        final String[] t2 = tok;
        IntStream.range(0, n).boxed().forEach(i -> mf.put(t2[i], i));
        long tForEach = System.nanoTime() - t0;

        t0 = System.nanoTime();
        final String[] t3 = tok;
        Map<String, Integer> mc = IntStream.range(0, n).boxed()
                .collect(Collectors.toMap(i -> t3[i], i -> i));
        long tToMap = System.nanoTime() - t0;

        System.out.println("STREAMDECOMP n=" + n
                + " traverse_ms=" + (tTraverse / 1e6)
                + " put_ms=" + (tPut / 1e6)
                + " merge_ms=" + (tMerge / 1e6)
                + " stream_foreach_put_ms=" + (tForEach / 1e6)
                + " tomap_ms=" + (tToMap / 1e6)
                + " sizes=" + traversed + "/" + mp.size() + "/" + mm.size()
                + "/" + mf.size() + "/" + mc.size());
    }
}
