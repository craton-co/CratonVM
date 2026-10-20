// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;

/**
 * gcg g2/r (2026-10-01): the entry ticket of
 * {@code common-d-proposal-marker-discovered-references} as a probe. The
 * w5-d scratch probe it names ({@code new WeakReference<>(new Object(), q);
 * System.gc();}) was never checked in; this is it, with the phantom, soft and
 * subclass shapes that a discovery-by-class-shape design must also cover.
 *
 * <p>Today the reference processor learns of a {@code Reference} only from the
 * constructor bridges in {@code native-builtins/src/reference.rs}. With those
 * shadowed ({@code CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/ref/}, the real
 * constructor bytecode runs) nothing is registered, so nothing is cleared or
 * enqueued: that arm is this probe's POSITIVE CONTROL for the defect (it must
 * print {@code false} fields today) and its pass condition once discovery no
 * longer needs the bridges.
 *
 * <p>Stdout (deterministic; HotSpot 25 {@code -XX:+UseSerialGC} prints it):
 * <pre>
 *   weak cleared=true enqueued=true polled=true
 *   phantom enqueued=true polled=true
 *   subclass cleared=true enqueued=true polled=true
 *   null-referent enqueued=false
 *   soft-recent kept=true
 *   ref-discovery-gate PROBE-OK
 * </pre>
 * A failing field prints {@code PROBE-FAIL} on the last line instead.
 */
public final class GcgR2RefDiscoveryGateProbe {

    /** A user subclass: discovery by class shape must cover it too. */
    static final class Tagged extends WeakReference<Object> {
        final int tag;

        Tagged(Object referent, ReferenceQueue<Object> q, int tag) {
            super(referent, q);
            this.tag = tag;
        }
    }

    public static void main(String[] args) throws Exception {
        boolean ok = true;

        ReferenceQueue<Object> wq = new ReferenceQueue<>();
        WeakReference<Object> weak = makeWeak(wq);
        ReferenceQueue<Object> pq = new ReferenceQueue<>();
        PhantomReference<Object> phantom = makePhantom(pq);
        ReferenceQueue<Object> tq = new ReferenceQueue<>();
        Tagged tagged = makeTagged(tq);
        ReferenceQueue<Object> nq = new ReferenceQueue<>();
        WeakReference<Object> nullReferent = new WeakReference<>(null, nq);
        SoftReference<Object> soft = makeSoft();

        boolean weakCleared = false;
        boolean taggedCleared = false;
        for (int attempt = 0; attempt < 20 && !(weakCleared && taggedCleared); attempt++) {
            System.gc();
            weakCleared = weak.refersTo(null);
            taggedCleared = tagged.refersTo(null);
            if (!(weakCleared && taggedCleared)) {
                Thread.sleep(20);
            }
        }
        Reference<?> wPolled = weakCleared ? wq.remove(2000) : wq.poll();
        boolean wEnq = wPolled == weak;
        System.out.println("weak cleared=" + weakCleared + " enqueued=" + wEnq + " polled=" + (wPolled != null));
        ok &= weakCleared && wEnq;

        Reference<?> pPolled = pq.remove(2000);
        boolean pEnq = pPolled == phantom;
        System.out.println("phantom enqueued=" + pEnq + " polled=" + (pPolled != null));
        ok &= pEnq;

        Reference<?> tPolled = taggedCleared ? tq.remove(2000) : tq.poll();
        boolean tEnq = tPolled == tagged && ((Tagged) tPolled).tag == 7;
        System.out.println("subclass cleared=" + taggedCleared + " enqueued=" + tEnq + " polled=" + (tPolled != null));
        ok &= taggedCleared && tEnq;

        // A Reference constructed with a null referent is never discovered
        // and never enqueued by the collector.
        System.gc();
        boolean nEnq = nq.remove(200) != null;
        System.out.println("null-referent enqueued=" + nEnq);
        ok &= !nEnq && nullReferent.refersTo(null);

        // A soft reference touched just now is kept (LRU policy).
        boolean kept = soft.get() != null;
        System.out.println("soft-recent kept=" + kept);
        ok &= kept;

        System.out.println("ref-discovery-gate " + (ok ? "PROBE-OK" : "PROBE-FAIL"));
    }

    // Each referent is created in its own frame, so no slot of main keeps it.
    static WeakReference<Object> makeWeak(ReferenceQueue<Object> q) {
        return new WeakReference<>(new Object(), q);
    }

    static PhantomReference<Object> makePhantom(ReferenceQueue<Object> q) {
        return new PhantomReference<>(new int[8], q);
    }

    static Tagged makeTagged(ReferenceQueue<Object> q) {
        return new Tagged(new long[2], q, 7);
    }

    static SoftReference<Object> makeSoft() {
        return new SoftReference<>(new byte[16]);
    }
}
