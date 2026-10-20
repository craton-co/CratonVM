// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gch h4/wr (2026-10-09): does a {@code System.gc()} clear a weak reference
 * whose YOUNG referent only a DEAD old object reaches?
 *
 * <p>The deciding probe of
 * {@code docs/internal/gc/gch-h3jx-gen-true-root-major-keeps-unstarted-compat-threads-one-cycle-FIXED-20261012.md}
 * in its smallest form. HotSpot Serial's full collection marks from the true
 * roots, so every such referent is cleared by the first {@code System.gc()}.
 * A CratonVM Generational requested major takes the true-root young seed
 * (gcd d4/n), but that seed also seeds the WATCHED set (gcd d5/r: every
 * address a weak table hands back after the pause, the referents included),
 * so a watched young survivor the young phase kept for a dead holder's card
 * stays set for one more major. {@code CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD=1}
 * judges such a survivor dead instead.
 *
 * <p>Shape, per pair i: an old holder H_i and an old target T_i (both tenured
 * during setup); then a fresh young child Y_i naming T_i is made the referent of
 * a weak reference W_i and stored only in H_i; H_i and T_i's slot are dropped;
 * one {@code System.gc()}. Every Y_i is unreachable. A W_i that still answers
 * after that collection is STALE; a stale Y_i whose T_i no longer reads its
 * fill pattern is BAD (a use-after-free: the old T_i was freed under a handed
 * back Y_i -- run with {@code CRATONVM_DBG_SWEEP_ZERO=1} so a freed block reads
 * zero).
 *
 * <p>stdout (HotSpot 25 {@code -XX:+UseSerialGC -Xmx64m}, with or without
 * {@code -XX:MaxTenuringThreshold=0}):
 * <pre>
 *   watched-young-dead: ok
 * </pre>
 * {@code ok} allows a sixteenth of the pairs to stay set (a conservative stack
 * word may hold a child); a VM that keeps them all prints {@code STALE}, and a
 * corrupted target appends {@code BAD}. Counts go to stderr
 * ({@code wr4 wyd pairs= stale= bad= stale_after_second=}).
 */
public class GchWr4WatchedYoungDeadProbe {
    static final int PAIRS = 512;
    static final int TARGET_LEN = 256;

    static final class Holder {
        Object child;
    }

    static final class Child {
        final byte[] target;

        Child(byte[] target) {
            this.target = target;
        }
    }

    static Holder[] holders = new Holder[PAIRS];
    static byte[][] targets = new byte[PAIRS][];
    @SuppressWarnings("unchecked")
    static WeakReference<Child>[] weaks = new WeakReference[PAIRS];
    static volatile Object sink;

    static void setUp() {
        for (int i = 0; i < PAIRS; i++) {
            holders[i] = new Holder();
            byte[] t = new byte[TARGET_LEN];
            java.util.Arrays.fill(t, (byte) i);
            targets[i] = t;
        }
        // Tenure the holders and the targets: one requested full collection,
        // then enough young churn to age anything it left young past every
        // tenuring threshold, then a second requested collection.
        System.gc();
        for (int i = 0; i < 800_000; i++) {
            sink = new byte[1024];
        }
        sink = null;
        System.gc();
    }

    /** Pair i: H_i -> fresh young Y_i (W_i's referent) -> T_i; then H_i and T_i's slot die. */
    static void arm(int i) {
        Child y = new Child(targets[i]);
        weaks[i] = new WeakReference<>(y);
        holders[i].child = y;
        targets[i] = null;
        holders[i] = null;
    }

    /** {stale, bad}: read in a frame of its own, so no child outlives it in a local. */
    static int[] census() {
        int stale = 0;
        int bad = 0;
        for (int i = 0; i < PAIRS; i++) {
            Child c = weaks[i].get();
            if (c != null) {
                stale++;
                byte[] t = c.target;
                if (t == null || t.length != TARGET_LEN || t[0] != (byte) i || t[TARGET_LEN - 1] != (byte) i) {
                    bad++;
                }
            }
        }
        return new int[] {stale, bad};
    }

    public static void main(String[] args) {
        setUp();
        for (int i = 0; i < PAIRS; i++) {
            arm(i);
        }
        System.gc();
        int[] first = census();
        System.gc();
        int[] second = census();
        System.err.println("wr4 wyd pairs=" + PAIRS + " stale=" + first[0] + " bad=" + first[1]
                + " stale_after_second=" + second[0] + " bad_after_second=" + second[1]);
        String verdict = first[0] * 16 <= PAIRS ? "ok" : "STALE";
        if (first[1] + second[1] != 0) {
            verdict += " BAD";
        }
        System.out.println("watched-young-dead: " + verdict);
    }
}
