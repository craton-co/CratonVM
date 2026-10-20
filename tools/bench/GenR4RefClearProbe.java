// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * gen r4/mark (2026-09-23): a {@code Reference} the APPLICATION cleared must
 * stay cleared, and must never be enqueued by the collector.
 *
 * <p>{@code Reference.clear()} only writes the referent slot. CratonVM's
 * reference processor still holds the entry with the old referent address,
 * the pre-GC pass nulls the (already null) slot, and the post-GC restore pass
 * writes the referent BACK whenever it survived. So {@code w.clear();
 * System.gc(); w.get()} returns the object again. If the referent later dies,
 * the processor also clears-and-enqueues a reference the application had
 * cleared itself. HotSpot never discovers a Reference whose referent is null.
 *
 * <p>Deterministic under HotSpot: every line below prints {@code ok}. Oracle:
 * <pre>
 *   java -cp tools/bench GenR4RefClearProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -cp tools/bench GenR4RefClearProbe
 * </pre>
 * See docs/internal/gc/gengc-r4-mark-mutator-clear-is-undone-by-restore-FIXED-20260923.md.
 */
public final class GenR4RefClearProbe {
    static Object keep; // strongly reachable referent

    public static void main(String[] args) throws Exception {
        int failures = 0;

        // 1. clear() on a weak ref whose referent stays strongly reachable.
        keep = new int[16];
        WeakReference<Object> w = new WeakReference<>(keep);
        w.clear();
        for (int i = 0; i < 3; i++) System.gc();
        failures += check("weak-cleared-stays-cleared", w.get() == null && !w.refersTo(keep));

        // 2. clear() on a weak ref registered with a queue, then the referent
        //    dies: the collector must NOT enqueue it (HotSpot never discovers it).
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        Object dying = new int[16];
        WeakReference<Object> wq = new WeakReference<>(dying, q);
        wq.clear();
        dying = null;
        for (int i = 0; i < 3; i++) System.gc();
        Reference<?> polled = q.remove(200);
        failures += check("cleared-weak-not-enqueued-by-gc", polled == null);

        // 3. Same for a phantom reference whose referent stays reachable.
        Object held = new int[16];
        PhantomReference<Object> p = new PhantomReference<>(held, new ReferenceQueue<>());
        p.clear();
        for (int i = 0; i < 3; i++) System.gc();
        failures += check("phantom-cleared-stays-cleared", !p.refersTo(held));
        java.lang.ref.Reference.reachabilityFence(held);

        System.out.println(failures == 0 ? "PASS" : "FAIL " + failures);
        if (failures != 0) System.exit(1);
    }

    static int check(String name, boolean ok) {
        System.out.println(name + ": " + (ok ? "ok" : "FAILED"));
        return ok ? 0 : 1;
    }
}
