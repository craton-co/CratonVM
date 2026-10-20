// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcj j3/x2: fail-first probe of the parity risk in
// docs/known-issues/gc/gcj-j3x2-wave-j2-default-on-review-residuals-20261019.md (item R1):
// `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS` (default ON) also defers the
// finalizables already QUEUED for finalize() (`finalizer_thread.pending_addresses()`
// joins the candidates, gc_and_alloc.rs `finalizable_roots`).
//
// In HotSpot a queued Finalizer is strongly reachable from Finalizer.queue, so the
// OLD child of a queued finalizable Q stays strongly reachable at the next full GC,
// and a WeakReference to that child is NOT cleared while Q waits in the queue.
// With the deferral, Q is marked only in the major's resurrection round, the child is
// in the depth-2 set, and the weak reference is cleared before Q.finalize() runs.
//
// Shape: the child is tenured by a System.gc() (Serial full GC promotes every
// survivor). A blocker B dies first and its finalize() parks the finalizer thread.
// Then Q (holding the child) dies in a young collection and is queued behind B. The
// child's last strong root is dropped, System.gc() runs, and the weak reference is
// read while Q is still queued. B is then released and Q.finalize() checks its child.
//
// stdout (deterministic; HotSpot 25 -XX:+UseSerialGC):
//   fact blocker-entered true
//   fact weak-alive-while-queued true
//   fact queued-finalized 1
//   fact child-intact true
// stderr: diagnostics (collection counts, waits).
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public class Gcj3X2QueuedFinalizableOldChildProbe {
    static final CountDownLatch ENTERED = new CountDownLatch(1);
    static final CountDownLatch RELEASE = new CountDownLatch(1);
    static volatile int queuedFinalized;
    static volatile boolean childIntact;
    static volatile Object sink;
    static Child childRoot;

    static final class Child {
        final int[] payload = new int[] {3, 5, 9};
    }

    static final class Blocker {
        @Override
        @SuppressWarnings({"deprecation", "removal"})
        protected void finalize() throws InterruptedException {
            ENTERED.countDown();
            RELEASE.await(30, TimeUnit.SECONDS);
        }
    }

    static final class Queued {
        Child child;

        @Override
        @SuppressWarnings({"deprecation", "removal"})
        protected void finalize() {
            Child c = child;
            childIntact = c != null
                    && c.payload.length == 3
                    && c.payload[0] + c.payload[1] + c.payload[2] == 17;
            queuedFinalized = queuedFinalized + 1;
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

    static void dropBlocker() {
        new Blocker();
    }

    static WeakReference<Child> dropQueued() {
        Queued q = new Queued();
        q.child = childRoot;
        return new WeakReference<>(q.child);
    }

    public static void main(String[] args) throws Exception {
        childRoot = new Child();
        System.gc();
        // B dies in a young collection; its finalize() parks the finalizer thread.
        dropBlocker();
        boolean entered = false;
        for (int i = 0; i < 40 && !entered; i++) {
            oneCollection();
            entered = ENTERED.await(250, TimeUnit.MILLISECONDS);
        }
        System.out.println("fact blocker-entered " + entered);
        // Q dies in a young collection and is queued behind B.
        WeakReference<Child> w = dropQueued();
        oneCollection();
        oneCollection();
        long gcBefore = gcCount();
        childRoot = null;
        System.gc();
        boolean alive = w.get() != null;
        System.out.println("fact weak-alive-while-queued " + alive);
        System.err.println("gcs=" + gcBefore + "->" + gcCount() + " q-finalized-early=" + queuedFinalized);
        RELEASE.countDown();
        for (int i = 0; i < 200 && queuedFinalized == 0; i++) {
            System.runFinalization();
            Thread.sleep(25);
        }
        System.out.println("fact queued-finalized " + queuedFinalized);
        System.out.println("fact child-intact " + childIntact);
    }
}
