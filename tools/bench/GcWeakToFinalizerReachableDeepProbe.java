// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.lang.ref.SoftReference;
import java.util.WeakHashMap;

/**
 * The depth-2 companion of {@code GenR4WeakToFinalizableProbe}: weak
 * references to objects reachable ONLY through a pending finalizer (at depth
 * 2, 4 and 5, plus a {@code WeakHashMap} key) are cleared before
 * {@code finalize()} runs, while a weak reference (and a {@code WeakHashMap}
 * key) to an object the finalizable shares with a strong root stays set --
 * the collectors' resurrection closure must never name a strongly reachable
 * object. {@code old} promotes the graph before dropping it.
 *
 * <p>HotSpot (Serial, G1, Z): {@code PASS} for both arguments.
 * <pre>
 *   cratonvm --java-home "$JDK" -XX:+UseG1GC -Xmx256m -cp tools/bench GcWeakToFinalizerReachableDeepProbe young
 *   cratonvm --java-home "$JDK" -XX:+UseZGC  -Xmx256m -cp tools/bench GcWeakToFinalizerReachableDeepProbe old
 * </pre>
 * See docs/internal/gc-common-round-20260923/common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot-FIXED-20261010.md.
 */
public final class GcWeakToFinalizerReachableDeepProbe {
    static volatile int finalizedCount;
    static final Object strongShared = new Object();
    static final Object[] keep = new Object[1];

    static final class Node { Object next; int[] pad = new int[4]; }
    static final class Fin {
        Node head; Object shared;
        @SuppressWarnings({"deprecation", "removal"})
        @Override protected void finalize() { finalizedCount++; keep[0] = this; }
    }

    public static void main(String[] args) throws Exception {
        boolean old = args.length > 0 && args[0].equals("old");
        Fin f = new Fin();
        Node a = new Node(); Node b = new Node(); Node c = new Node();
        a.next = b; b.next = c; c.next = new int[3];
        f.head = a; f.shared = strongShared;
        WeakReference<Node> wa = new WeakReference<>(a);
        WeakReference<Node> wc = new WeakReference<>(c);
        WeakReference<Object> wcc = new WeakReference<>(c.next);
        WeakReference<Object> wshared = new WeakReference<>(strongShared);
        WeakHashMap<Object, String> whm = new WeakHashMap<>();
        whm.put(b, "b"); whm.put(strongShared, "s");
        a = null; b = null; c = null;
        if (old) {
            for (int i = 0; i < 6; i++) { System.gc(); Thread.sleep(5); }
        }
        f = null;
        for (int i = 0; i < 20 && finalizedCount == 0; i++) {
            System.gc(); System.runFinalization(); Thread.sleep(20);
        }
        if (finalizedCount == 0) { System.out.println("INCONCLUSIVE"); System.exit(2); }
        System.gc(); System.runFinalization();
        int fail = 0;
        fail += check("depth2-cleared", wa.get() == null);
        fail += check("depth4-cleared", wc.get() == null);
        fail += check("depth5-cleared", wcc.get() == null);
        fail += check("strong-shared-kept", wshared.get() == strongShared);
        fail += check("whm-size-1", whm.size() == 1);
        fail += check("whm-strong-kept", "s".equals(whm.get(strongShared)));
        fail += check("finalized-once", finalizedCount == 1);
        System.out.println(fail == 0 ? "PASS" : "FAIL " + fail);
        System.exit(fail == 0 ? 0 : 1);
    }
    static int check(String n, boolean ok) { System.out.println(n + ": " + (ok ? "ok" : "FAILED")); return ok ? 0 : 1; }
}
