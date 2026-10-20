// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcj j1/rf: the fail-first probe of
// docs/internal/gc/gci-i47w-ec47-gen-major-marks-an-old-child-of-a-resurrected-finalizable-strongly-RETIRED-UNFIXED-20261018.md
//
// A dead YOUNG finalizable Fin privately holds an OLD Child (tenured first:
// run with -XX:MaxTenuringThreshold=0). A WeakReference to the child is the
// observable. HotSpot 25 (Serial full GC) clears weak references before it
// marks the finalizer-reachable set, so the System.gc() that finds Fin dead
// clears the child's weak reference before finalize() runs, whatever
// generation the child is in. The CratonVM Generational major marks an old
// child of a resurrected young finalizable from the young seed as a ROOT, so
// the weak reference survives that collection and clears one major late,
// after finalize() (cleared-at-gc false, cleared-before-finalize false).
//
// Modes (first argument): `old` (the shape above), `young` (the child stays
// young: the control that both VMs already agree on), `reach` (Fin is also
// held by a static: never finalized, the weak reference never cleared).
//
// stdout (deterministic; HotSpot 25 -XX:+UseSerialGC -XX:MaxTenuringThreshold=0):
//   old / young: fact cleared-at-gc true
//                fact cleared-before-finalize true
//                fact child-intact true
//                fact finalized 1
//   reach:       fact cleared-at-gc false
//                fact finalized 0
// stderr: diagnostics.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;

public class Gcj1RfFinalizableOldChildWeak {
    static volatile int finalized;
    static volatile boolean childIntact;
    static volatile boolean clearedBeforeFinalize;
    static volatile WeakReference<Child> childWeak;
    static volatile Object sink;
    static Child childStash;
    static final Object[] HOLD = new Object[1];

    static final class Child {
        final int[] payload = new int[] {3, 5, 9};
    }

    static final class Fin {
        Child child;

        @Override
        @SuppressWarnings({"deprecation", "removal"})
        protected void finalize() {
            Child c = child;
            childIntact = c != null
                    && c.payload.length == 3
                    && c.payload[0] + c.payload[1] + c.payload[2] == 17;
            WeakReference<Child> w = childWeak;
            clearedBeforeFinalize = w != null && w.get() == null;
            finalized = finalized + 1;
        }
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = b.getCollectionCount();
            if (c > 0) {
                n += c;
            }
        }
        return n;
    }

    /** Allocate until at least one more collection has run (bounded, 20 s). */
    static void oneCollection() {
        long before = gcCount();
        Object[] ring = new Object[64];
        int n = 0;
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (gcCount() == before && System.nanoTime() < deadline) {
            for (int i = 0; i < 1024; i++) {
                ring[n++ & 63] = new byte[2048];
            }
        }
        sink = ring;
    }

    /** The finalizable and the weak reference; only the weak reference leaves. */
    static void fresh(String mode) {
        Fin f = new Fin();
        Child c = mode.equals("young") ? new Child() : childStash;
        childStash = null;
        f.child = c;
        childWeak = new WeakReference<>(c);
        if (mode.equals("reach")) {
            HOLD[0] = f;
        }
    }

    static boolean gone(WeakReference<?> w) {
        return w.get() == null;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "old";
        if (!mode.equals("young")) {
            childStash = new Child();
            // Tenure the child (MaxTenuringThreshold=0: the first young
            // collection promotes it), then a second one for good measure.
            oneCollection();
            oneCollection();
        }
        fresh(mode);
        WeakReference<Child> wc = childWeak;
        System.gc();
        boolean clearedAtGc = gone(wc);
        int waits = 0;
        int maxWaits = mode.equals("reach") ? 6 : 200;
        while (finalized == 0 && waits < maxWaits) {
            System.runFinalization();
            Thread.sleep(25);
            waits++;
        }
        System.err.println("[gcj1rf-ec47] mode=" + mode + " waits=" + waits
                + " gcs=" + gcCount());
        System.out.println("fact cleared-at-gc " + clearedAtGc);
        if (!mode.equals("reach")) {
            System.out.println("fact cleared-before-finalize " + clearedBeforeFinalize);
            System.out.println("fact child-intact " + childIntact);
        }
        System.out.println("fact finalized " + finalized);
    }
}
