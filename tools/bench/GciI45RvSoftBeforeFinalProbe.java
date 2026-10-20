// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.concurrent.atomic.AtomicIntegerArray;

/**
 * gci i45/rv ({@code docs/known-issues/gc/gci-i41rv-waves-i39-i40-review-residuals-20261016.md}
 * item 4, the deciding row of the w36-d ordering question the i40/gn in-place
 * resurrection closure inherits): HotSpot processes SoftReferences BEFORE
 * FinalReferences. An object {@code X} reachable from a dead finalizable
 * {@code Fin} AND from a {@code SoftReference} the soft-reference policy keeps
 * (a heap far from full, a soft reference created moments ago) is kept alive by
 * the soft phase, so it is strongly reachable when the weak phase runs and a
 * {@code WeakReference(X)} is NOT cleared. A collector that traces the
 * finalizer closure before (or without) the kept soft referents files {@code X}
 * as "finalizer-reachable only" and clears the weak reference: the defect the
 * page asks about.
 *
 * <p>Control: {@code Y}, reachable from the same {@code Fin} only. HotSpot
 * clears a {@code WeakReference(Y)} at the pause that finds {@code Fin} dead
 * (weak before final), and keeps it cleared after {@code finalize()} ran.
 *
 * <pre>
 *   java     -XX:+UseSerialGC -Xmx256m -cp &lt;classes&gt; GciI45RvSoftBeforeFinalProbe [trials] [timeoutMs]
 *   cratonvm -XX:+UseGenerationalGC -Xmx256m -cp &lt;classes&gt; GciI45RvSoftBeforeFinalProbe [trials] [timeoutMs]
 * </pre>
 *
 * Each trial (default 10) builds {@code Fin -> X, Y} in a separate frame
 * ({@link #make}; nothing but the finalizer registration holds {@code Fin}
 * after it returns), keeps {@code SoftReference(X)}, {@code WeakReference(X)}
 * and {@code WeakReference(Y)} in static arrays, then allocates short-lived
 * garbage (young collections, no {@code System.gc()}) until {@code finalize()}
 * has run for that trial or {@code timeoutMs} (default 15000) passed. A trial
 * whose {@code finalize()} ran is JUDGED, right then:
 * <ul>
 *   <li>VIOLATION when {@code WeakReference(X)} reads {@code null} while
 *       {@code SoftReference(X)} still holds X (a soft referent the policy
 *       kept, yet the weak reference to it was cleared), or when both read
 *       {@code null} (the soft reference was cleared on a heap this far from
 *       full: not HotSpot's policy answer either);</li>
 *   <li>CONTROL-KEPT when {@code WeakReference(Y)} still holds Y.</li>
 * </ul>
 *
 * stdout (deterministic; HotSpot 25 prints exactly this):
 * <pre>
 *   soft-before-final judged-any true
 *   soft-before-final violations 0
 *   finalizer-only-weak control-kept 0
 * </pre>
 * stderr (not compared): {@code [i45rv] trials=T judged=J violations=V
 * soft-cleared=S control-kept=C young-only=K timed-out=O ms=..}, where
 * {@code young-only} counts judged trials during which no non-young collector
 * ran (the shape the page asks about: the pause that found {@code Fin} dead
 * was a young one), and one {@code [i45rv] trial ..} line per judged trial.
 */
public class GciI45RvSoftBeforeFinalProbe {
    static final class Payload {
        final byte[] pad = new byte[48];
        final int tag;

        Payload(int tag) {
            this.tag = tag;
        }
    }

    static final class Fin {
        final Payload x;
        final Payload y;
        final int trial;

        Fin(Payload x, Payload y, int trial) {
            this.x = x;
            this.y = y;
            this.trial = trial;
        }

        @Override
        @SuppressWarnings("deprecation")
        protected void finalize() {
            FINALIZED.set(trial, 1);
        }
    }

    static AtomicIntegerArray FINALIZED;
    static SoftReference<Payload>[] SOFT;
    static WeakReference<Payload>[] WEAK_X;
    static WeakReference<Payload>[] WEAK_Y;
    static volatile Object sink;

    /** Builds {@code Fin -> X, Y} and the three references; holds nothing after it returns. */
    static void make(int t) {
        Payload x = new Payload(2 * t);
        Payload y = new Payload(2 * t + 1);
        SOFT[t] = new SoftReference<>(x);
        WEAK_X[t] = new WeakReference<>(x);
        WEAK_Y[t] = new WeakReference<>(y);
        new Fin(x, y, t);
    }

    static boolean isYoungCollector(String name) {
        return name.equals("Copy")
                || name.equals("G1 Young Generation")
                || name.equals("PS Scavenge")
                || name.equals("ParNew");
    }

    static long nonYoungCollections() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (!isYoungCollector(gc.getName())) {
                n += Math.max(0, gc.getCollectionCount());
            }
        }
        return n;
    }

    static long youngCollections() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (isYoungCollector(gc.getName())) {
                n += Math.max(0, gc.getCollectionCount());
            }
        }
        return n;
    }

    /** Short-lived garbage until trial {@code t} is finalized or the deadline passes. */
    static boolean churnUntilFinalized(int t, long deadlineNanos) {
        Object[] ring = new Object[64];
        int i = 0;
        while (FINALIZED.get(t) == 0) {
            for (int k = 0; k < 4096; k++) {
                ring[(i++) & 63] = new byte[96];
            }
            sink = ring[i & 63];
            if (System.nanoTime() > deadlineNanos) {
                return false;
            }
            Thread.yield();
        }
        return true;
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) {
        int trials = args.length > 0 ? Integer.parseInt(args[0]) : 10;
        long timeoutMs = args.length > 1 ? Long.parseLong(args[1]) : 15_000L;
        FINALIZED = new AtomicIntegerArray(trials);
        SOFT = new SoftReference[trials];
        WEAK_X = new WeakReference[trials];
        WEAK_Y = new WeakReference[trials];
        long t0 = System.nanoTime();
        int judged = 0;
        int violations = 0;
        int softCleared = 0;
        int controlKept = 0;
        int youngOnly = 0;
        int timedOut = 0;
        for (int t = 0; t < trials; t++) {
            long oldBefore = nonYoungCollections();
            long youngBefore = youngCollections();
            make(t);
            boolean finalized =
                    churnUntilFinalized(t, System.nanoTime() + timeoutMs * 1_000_000L);
            if (!finalized) {
                timedOut++;
                System.err.println("[i45rv] trial " + t + " timed-out (finalize() never ran)");
                continue;
            }
            judged++;
            boolean weakX = WEAK_X[t].get() != null;
            boolean softX = SOFT[t].get() != null;
            boolean weakY = WEAK_Y[t].get() != null;
            long oldDelta = nonYoungCollections() - oldBefore;
            long youngDelta = youngCollections() - youngBefore;
            boolean violation = !weakX;
            if (violation) {
                violations++;
            }
            if (!softX) {
                softCleared++;
            }
            if (weakY) {
                controlKept++;
            }
            if (oldDelta == 0) {
                youngOnly++;
            }
            System.err.println("[i45rv] trial " + t + " weak-x=" + (weakX ? "set" : "cleared")
                    + " soft-x=" + (softX ? "set" : "cleared")
                    + " weak-y=" + (weakY ? "set" : "cleared")
                    + " young=" + youngDelta + " non-young=" + oldDelta);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("soft-before-final judged-any " + (judged > 0));
        System.out.println("soft-before-final violations " + violations);
        System.out.println("finalizer-only-weak control-kept " + controlKept);
        System.err.println("[i45rv] trials=" + trials + " judged=" + judged
                + " violations=" + violations + " soft-cleared=" + softCleared
                + " control-kept=" + controlKept + " young-only=" + youngOnly
                + " timed-out=" + timedOut + " ms=" + ms);
    }
}
