// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L7: ns per loop iteration for the
// dispatch loop's packed gate words
// (i77-L7-proposal-pack-the-dispatch-loops-diagnostic-gates-into-one-word-
// 20261110):
//   * unconditional (cross-binary against the wave-81 binary): fifteen
//     loop-carried bools packed into `diag_gates` / `door_gates` /
//     `top_gates`; every row should be flat or better, and the loop frame
//     (`tools/scripts/internal/loop-symbol-ratchet.sh`) about 8-16 bytes
//     smaller;
//   * CRATONVM_INTERP_STACK_DUMP_AT_POLL (default off): the stack-dump hook
//     leaves the per-bytecode path. It changes anything only while a watchdog
//     or sampler is armed, so A/B it with CRATONVM_DEFAULT_WATCHDOG_SEC=3600
//     (armed, never fires): with the watchdog and the switch off every
//     bytecode pays the hook's two dependent loads; with the switch on, none.
//
// Rows (ITERS = 2 000 000 iterations each):
//   intLoop     `s += i & 7` (locals only; the per-bytecode floor).
//   fieldLoop   `this.count += i & 3` (getfield / putfield arms).
//   invokeStat  `s += f(i)`, a static call (invokestatic door, return arm).
//   invokeVirt  `s += o.g(i)`, a monomorphic virtual call (invokevirtual
//               door, return arm).
//   arrayLoop   `a[i & 7] += 1` (array increment arms).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare env sets on the SAME
// binary (and the two binaries), interleaved, several rounds, and take the
// median of the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W82GateWordBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of N)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// intLoop checksum=7000000
// fieldLoop checksum=3000000
// invokeStat checksum=1000000
// invokeVirt checksum=3000000
// arrayLoop checksum=9000000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W82GateWordBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 2_000_000;

    int count;

    static class Callee {
        int g(int x) {
            return x & 3;
        }
    }

    static int f(int x) {
        return x & 1;
    }

    /** Sum of `a[k] * (k + 1)`. */
    static int weigh(int[] a) {
        int s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return s;
    }

    // 2 000 000 / 8 blocks of 0 + 1 + ... + 7 = 28: 7 000 000.
    static int intLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += i & 7;
        }
        return s;
    }

    // 2 000 000 / 4 blocks of 0 + 1 + 2 + 3 = 6: 3 000 000.
    int fieldLoop() {
        count = 0;
        for (int i = 0; i < ITERS; i++) {
            count += i & 3;
        }
        return count;
    }

    // Half the iterations are odd: 1 000 000.
    static int invokeStat() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += f(i);
        }
        return s;
    }

    // As fieldLoop: 3 000 000.
    static int invokeVirt(Callee o) {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += o.g(i);
        }
        return s;
    }

    // Each of the 8 elements ends at 250 000; weighed: 250 000 * 36.
    static int arrayLoop(int[] a) {
        java.util.Arrays.fill(a, 0);
        for (int i = 0; i < ITERS; i++) {
            a[i & 7] += 1;
        }
        return weigh(a);
    }

    static void time(String name, Row row) {
        int checksum = row.run(); // untimed warm-up
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            int c = row.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        L7W82GateWordBench self = new L7W82GateWordBench();
        Callee callee = new Callee();
        int[] arr = new int[8];
        time("intLoop", L7W82GateWordBench::intLoop);
        time("fieldLoop", self::fieldLoop);
        time("invokeStat", L7W82GateWordBench::invokeStat);
        time("invokeVirt", () -> invokeVirt(callee));
        time("arrayLoop", () -> arrayLoop(arr));
    }
}
