// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 84, lane L7: ns per loop iteration for the
// dispatch loop's one top-of-loop signal test
// (CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST, default off;
// interpreter-i84-L7-proposal-one-loop-top-test-for-both-pending-signals-KEPT-OFF-20261119).
// With the switch on, every bytecode tests one bit of `top_gates` for both
// parked signals and the stack-dump hook; off, it also loads the two
// pending slots (the pre-wave-84 test). Also cross-binary against wave 83
// for the unconditional half of the wave (the parked `RuntimeError` boxed,
// the pair-census cursor boxed: a smaller loop frame).
//
// Rows:
//   intLoop    `s += i & 7` (ITERS iterations; the per-bytecode floor).
//   mixLoop    `s += (i ^ (i >>> 3)) & 15` (ITERS; more straight-line
//              bytecodes per iteration, so more loop tops per back edge).
//   throwLoop  `s += a[i & 7]` over a 7-element array, the AIOOBE of every
//              eighth iteration caught in place (THROW_ITERS; the parked
//              error's own path: park, the top's test, the box, the unwind).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the switch off and on
// on the SAME binary, interleaved, several rounds, and take the median of
// the per-round minima (in-JVM timings swing ~3x between reps on the
// Windows host). Also try it with CRATONVM_DEFAULT_WATCHDOG_SEC=3600 (the
// stack-dump bit set on every bytecode), where both modes enter the cold
// block per bytecode and should tie.
//
// Run:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W84TopSignalBench
//   CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W84TopSignalBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of N)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// intLoop checksum=7000000
// mixLoop checksum=15000000
// throwLoop checksum=375000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W84TopSignalBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 2_000_000;
    static final int THROW_ITERS = 200_000;

    // 2 000 000 / 8 blocks of 0 + 1 + ... + 7 = 28: 7 000 000.
    static int intLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += i & 7;
        }
        return s;
    }

    // The term depends on i mod 128 only; one period sums to 960
    // (16 values of i >>> 3, eight of i & 7: 28 per period of the low three
    // bits, plus 64 for each of the eight periods whose bit 3 is set), and
    // 2 000 000 is 15 625 periods: 15 000 000.
    static int mixLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += (i ^ (i >>> 3)) & 15;
        }
        return s;
    }

    // Seven of eight iterations add 2, the eighth (index 7) throws and adds
    // 1: 175 000 * 2 + 25 000 = 375 000.
    static int throwLoop(int[] a) {
        int s = 0;
        for (int i = 0; i < THROW_ITERS; i++) {
            try {
                s += a[i & 7];
            } catch (ArrayIndexOutOfBoundsException e) {
                s += 1;
            }
        }
        return s;
    }

    static void time(String name, int iters, Row row) {
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
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", name, (double) best / iters, REPS);
    }

    public static void main(String[] args) {
        int[] seven = {2, 2, 2, 2, 2, 2, 2};
        time("intLoop", ITERS, L7W84TopSignalBench::intLoop);
        time("mixLoop", ITERS, L7W84TopSignalBench::mixLoop);
        time("throwLoop", THROW_ITERS, () -> throwLoop(seven));
    }
}
