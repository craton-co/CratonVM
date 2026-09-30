// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gen r4/mark (2026-09-23): weak references to an object that is only
 * reachable through a pending finalizer must be cleared BEFORE finalization,
 * and stay cleared even if {@code finalize()} resurrects the object.
 *
 * <p>The {@code java.lang.ref} package documentation: when the collector
 * determines an object is weakly reachable it clears all weak references to it
 * "and at the same time it will declare all of the formerly weakly-reachable
 * objects to be finalizable". HotSpot does exactly that: FinalReference
 * referents are kept alive only in the phase AFTER soft/weak clearing.
 *
 * <p>CratonVM's `ReferenceProcessor::process_references_with_finalizer_trace`
 * folds the finalizer-reachable closure (`finalizer_live`) into BOTH the soft
 * and the weak liveness predicates (the "V18" comment, which says HotSpot does
 * the same — it does not), so both weak references below are expected to
 * survive and to hand back the resurrected object.
 *
 * <p>Deterministic under HotSpot (finalization enabled, the default through at
 * least JDK 25): prints two {@code ok} lines and {@code PASS}.
 * <pre>
 *   java -cp tools/bench GenR4WeakToFinalizableProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -cp tools/bench GenR4WeakToFinalizableProbe
 * </pre>
 * See docs/internal/gc/gengc-r4-mark-weak-refs-honour-finalizer-reachability-RETIRED-20260923.md
 * (duplicate of docs/known-issues/gc/common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot.md).
 */
public final class GenR4WeakToFinalizableProbe {
    static volatile Finalizable resurrected;
    static volatile boolean finalized;

    static final class Finalizable {
        final int[] child = new int[8];

        @SuppressWarnings({"deprecation", "removal"})
        @Override
        protected void finalize() {
            resurrected = this;
            finalized = true;
        }
    }

    public static void main(String[] args) throws Exception {
        Finalizable f = new Finalizable();
        WeakReference<Finalizable> toObject = new WeakReference<>(f);
        WeakReference<int[]> toChild = new WeakReference<>(f.child);
        f = null;

        for (int i = 0; i < 20 && !finalized; i++) {
            System.gc();
            System.runFinalization();
            Thread.sleep(20);
        }
        if (!finalized) {
            System.out.println("INCONCLUSIVE: finalize() never ran");
            System.exit(2);
        }
        int failures = 0;
        failures += check("weak-to-finalizable-cleared", toObject.get() == null);
        failures += check("weak-to-finalizer-reachable-child-cleared", toChild.get() == null);
        System.out.println(failures == 0 ? "PASS" : "FAIL " + failures);
        if (failures != 0) System.exit(1);
    }

    static int check(String name, boolean ok) {
        System.out.println(name + ": " + (ok ? "ok" : "FAILED"));
        return ok ? 0 : 1;
    }
}
