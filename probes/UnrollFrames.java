// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Constant-trip counted loops whose bodies a safepoint snapshot names — the
 * population `CRATONVM_JIT_IR_PER_COPY_FRAMES` exists for.
 *
 * Three shapes, because the interesting question is not whether one unrolls but
 * which of them the per-copy machinery is actually reached by:
 *
 *   * {@link #pure} — arithmetic only. Unrolls either way once the frames are
 *     handled; nothing in it can deopt.
 *   * {@link #invariant} — one field read of a receiver that never changes. The
 *     copies' loads are the same load, so if any of them traps, copy 0 traps
 *     first and the later copies' frames are correct but unreached.
 *   * {@link #variant} — a pointer walk, so copy k's load is copy k's own. This
 *     is the shape where a frame from iteration k > 0 can actually be consulted.
 *
 * Prints a checksum, so an arm that computes differently says so rather than
 * merely running.
 */
public class UnrollFrames {
    static final class N {
        int v;
        N next;
        N(int v, N next) { this.v = v; this.next = next; }
    }

    int f = 7;

    static int pure() {
        int a = 0;
        for (int i = 0; i < 5; i++) a += i;
        return a;
    }

    static int invariant(UnrollFrames o) {
        int a = 0;
        for (int i = 0; i < 5; i++) a += o.f + i;
        return a;
    }

    static int variant(N o) {
        int a = 0;
        for (int i = 0; i < 5; i++) { a += o.v; o = o.next; }
        return a;
    }

    public static void main(String[] args) {
        int reps = Integer.getInteger("probe.reps", 50000);
        UnrollFrames self = new UnrollFrames();
        N head = null;
        for (int i = 5; i > 0; i--) head = new N(i, head);

        long sum = 0;
        for (int r = 0; r < reps; r++) {
            sum += pure();
            sum += invariant(self);
            sum += variant(head);
        }
        System.out.println("UnrollFrames checksum=" + sum);
    }
}
