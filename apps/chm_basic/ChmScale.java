// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.ConcurrentHashMap;

/**
 * W2-CHM probe: does a single-threaded {@code ConcurrentHashMap<String,Integer>}
 * return every value that was put into it?
 *
 * <p>Driven by {@code vm/tests/wave2_chm.rs}. Every number on every printed line
 * is READ BACK from the map — nothing here prints an expected literal — so a VM
 * that loses entries, or that hands back a boxed {@code Integer} carrying the
 * wrong {@code value}, prints different numbers and the test goes red.
 *
 * <h2>Why the stage sizes are what they are</h2>
 *
 * The defect this pins is a JIT miscompile of {@code Integer.valueOf(int)} /
 * {@code Integer.<init>(int)} as an allocate-then-putfield sequence that stores
 * the wrong value, so the wrapper comes back with {@code value=0}. It only
 * fires once the surrounding frame has crossed BOTH JIT thresholds:
 *
 * <ul>
 *   <li>the OSR back-edge threshold (1000) — reached inside a single
 *       {@link #fill} invocation only on the last stage, whose loop takes
 *       1000 back-edges;</li>
 *   <li>the per-callee invocation threshold (2000) for {@code Integer.valueOf}
 *       — the six warm-up stages autobox 16+32+64+128+256+512 = 1008 values,
 *       so the 2000th {@code Integer.valueOf} call of the run is the one for
 *       {@code k992} of the final stage.</li>
 * </ul>
 *
 * That is why the historical failure read
 * {@code size=1000 mapSize=1000 found=992 firstMiss=992}: the boxing goes wrong
 * exactly where the callee tips over. Do not re-order or trim the stage list —
 * the arithmetic above is the probe.
 */
public final class ChmScale {

    /**
     * Ascending stage sizes. The six warm-up stages sum to 1008 autoboxed
     * values; the final 1000-entry stage is the one whose fill loop crosses the
     * OSR back-edge threshold in a single invocation.
     */
    private static final int[] STAGES = {16, 32, 64, 128, 256, 512, 1000};

    /**
     * Fill loop, deliberately in its own method so the back-edge count per
     * invocation equals {@code n} and only the 1000-entry stage triggers OSR.
     * The {@code put} autoboxes, i.e. calls {@code Integer.valueOf(i)}.
     */
    private static void fill(ConcurrentHashMap<String, Integer> map, int n) {
        for (int i = 0; i < n; i++) {
            map.put("k" + i, i);
        }
    }

    /** Build a map of {@code n} entries, read every one back, report. */
    private static void stage(int n) {
        ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
        fill(map, n);

        int mapSize = map.size();
        int found = 0;
        int firstMiss = -1;
        for (int i = 0; i < n; i++) {
            Integer boxed = map.get("k" + i);
            if (boxed != null && boxed.intValue() == i) {
                found++;
            } else if (firstMiss < 0) {
                firstMiss = i;
            }
        }

        System.out.println("size=" + n
                + " mapSize=" + mapSize
                + " found=" + found
                + " firstMiss=" + firstMiss);
    }

    public static void main(String[] args) {
        for (int n : STAGES) {
            stage(n);
        }
    }
}
