// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;

/**
 * gci i42/rp: under the reference exclusion switch {@code CRATONVM_DBG_NO_REFPROC}
 * a weak reference to a STRONGLY held object must keep it (the switch promises
 * "weak refs never clear", never "every weak ref reads null"), and the finalizer
 * queue that keeps filling on that arm must survive moving collections.
 *
 * <p>Page: {@code docs/known-issues/gc/gci-i42rp-the-no-refproc-exclusion-arm-nulls-every-weak-referent-20261016.md}.
 * Usage: {@code GciI42RpNoRefprocWeakProbe} (no arguments). Stdout is
 * deterministic and HotSpot's whatever the variable says
 * ({@code live-weak-kept true}, {@code live-weak-same true}); the number of
 * collections survived goes to stderr. Garbage finalizables are churned through
 * young collections so a moving pause relocates the (undrained) finalizer queue.
 */
public class GciI42RpNoRefprocWeakProbe {
    static final class Fin {
        final byte[] pad = new byte[48];

        @Override
        @SuppressWarnings("deprecation")
        protected void finalize() {
        }
    }

    static int sink;

    public static void main(String[] args) {
        Object strong = new int[8];
        WeakReference<Object> weak = new WeakReference<>(strong);
        boolean kept = true;
        for (int round = 0; round < 20; round++) {
            ArrayList<Object> garbage = new ArrayList<>();
            for (int i = 0; i < 20_000; i++) {
                garbage.add(new byte[64]);
                if ((i & 63) == 0) {
                    new Fin();
                }
            }
            sink += garbage.size();
            garbage = null;
            if ((round & 3) == 0) {
                System.gc();
            }
            if (weak.get() != strong) {
                kept = false;
            }
        }
        System.out.println("live-weak-kept " + kept);
        System.out.println("live-weak-same " + (weak.get() == strong));
        System.err.println("[i42rp-norefproc] rounds=20 sink=" + (sink & 1));
    }
}
