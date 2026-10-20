// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gci i20/mr (`docs/known-issues/gc/gci-i20mr-merge-review-findings-20261015.md`
 * finding 2): on G1, {@code System.gc()} takes the stop-the-world full mark
 * cycle by default since the 2026-10-15 merge of origin/dev, and that door's
 * reference pass DEFERS its {@code -Xlog:gc+ref=debug} block; the door never
 * printed it (the concurrent remark does).
 *
 * <p>Each round {@code k} prints {@code window k begin}, runs one
 * {@code System.gc()}, prints {@code window k end}, then whether a weakly
 * reachable object was cleared. Run with {@code -Xlog:gc+ref=debug}: the
 * unified-logging lines go to stdout between the two markers, so the rule
 * counts the reference blocks inside each window. Without {@code -Xlog} the
 * stdout is deterministic on HotSpot 25 ({@code cleared=true} every round).
 *
 * <p>Args: {@code [rounds]} (default 3). Nothing on stderr.
 */
public class GciI20MrG1SystemGcRefBlockProbe {
    /** A weakly reachable object: nothing else keeps it once this returns. */
    static WeakReference<Object> make() {
        return new WeakReference<>(new Object());
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        for (int k = 1; k <= rounds; k++) {
            WeakReference<Object> w = make();
            System.out.println("window " + k + " begin");
            System.gc();
            System.out.println("window " + k + " end");
            System.out.println("weak " + k + " cleared=" + (w.get() == null));
        }
        System.out.println("done");
    }
}
