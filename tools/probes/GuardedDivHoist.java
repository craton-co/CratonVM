// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * What the LICM guard refusal cost, on the one shape where it can be priced:
 * a counted loop whose body holds an `Op::Guard` (the `idiv` divide-by-zero
 * guard on `d`) AND a loop-invariant read of `this.bias`. The guard constrains
 * `d`, not `this`, so `ir_optimize::LoopGuards` attributes it away and the read
 * is hoisted; with the attribution off the blanket refusal pins it.
 *
 * Four arms, same binary (`NOTES-opts8.md` section 4):
 *
 *   CRATONVM_DBG_LICM=1 cratonvm -cp probes GuardedDivHoist
 *   CRATONVM_JIT_IR_LICM_GUARD_ATTRIBUTION=0 ...
 *   CRATONVM_JIT_IR_GUARD_TOKEN=1 ...
 *   CRATONVM_JIT_IR_LICM_GUARD_ATTRIBUTION=0 CRATONVM_JIT_IR_GUARD_TOKEN=1 ...
 *
 * `read-hoist load N: HOIST` is the attribution's recovery; `skip -- base N
 * may be null ... or a guard may be what makes the read legal` is the refusal.
 * Expected checksum: r=362486571.
 *
 * Why not `String.charAt`: a method with a String access site is pinned to
 * the single-pass backend (`[ir] admission ...: pinned`), and with
 * `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` the optimizing tier still builds no
 * body for `GuardedFieldHoist`'s `run`. The guard machinery never runs there.
 */
public class GuardedDivHoist {
    int bias = 1;

    int run(int n, int d) {
        int c = 0;
        for (int i = 0; i < n; i++) c += i / d + bias;
        return c;
    }

    public static void main(String[] a) {
        GuardedDivHoist g = new GuardedDivHoist();
        int d = a.length + 3;
        for (int w = 0; w < 30000; w++) g.run(100, d);
        long t0 = System.nanoTime();
        int r = g.run(100_000_000, d);
        long t1 = System.nanoTime();
        System.out.printf("steady %.3f ns/iter r=%d%n", (t1 - t0) / 1e8, r);
    }
}
