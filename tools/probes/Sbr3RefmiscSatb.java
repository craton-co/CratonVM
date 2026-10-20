// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.Reference;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

/**
 * SBR round 3 wave 2, lane {@code refmisc}: the POSITIVE CONTROL for the SATB keep-alive of
 * {@code Reference.get()} (the native {@code gc_reference_keep_alive}; HotSpot's
 * {@code _Reference_get} intrinsic). Lane REF's {@code Sbr3RefGcStress} ran only young pauses, so
 * it passed with the keep-alive present AND absent. This one is built to fail without it.
 *
 * <p>The hazard needs a referent that is (1) OLD, so a young pause cannot clear it, (2) reachable
 * only through its weak/soft reference when a concurrent mark cycle takes its snapshot, and (3)
 * handed to the mutator by {@code get()} AFTER the snapshot, into a structure the marker has
 * already scanned. Then nothing marks it: the cycle's reference processing clears the reference
 * and the collector reclaims a live object. Each round:
 *
 * <ol>
 *   <li>build {@code N} payloads, each behind one reference, strongly held by {@code hold};
 *   <li>churn until they were promoted (several young pauses);
 *   <li>drop {@code hold}, leaving the payloads weakly reachable only;
 *   <li>wait a random delay (a mark cycle is, with a high probability, under way: ballast keeps
 *       the old generation above the initiating occupancy, a churn thread keeps it growing);
 *   <li>{@code get()} every reference into {@code store} (allocated before the cycle, so it is
 *       usually already scanned), then wait the cycle out and check every stored element.
 * </ol>
 *
 * Failure is any stored element that is not an intact {@code Payload} of its own index, or whose
 * reference was cleared while {@code store} still holds the referent.
 *
 * <pre>
 *   cratonvm --jdk-only -Xmx96m -XX:+UseG1GC -XX:InitiatingHeapOccupancyPercent=10 -cp . Sbr3RefmiscSatb [seconds] [weak|soft]
 *   -Xlog:gc shows the cycles; the line count of "concurrent" cycles is printed at the end when available
 * </pre>
 */
public final class Sbr3RefmiscSatb {
    static final class Payload {
        final int id;
        final long[] pad = new long[6];

        Payload(int id) {
            this.id = id;
            for (int i = 0; i < pad.length; i++) {
                pad[i] = id * 31L + i;
            }
        }

        boolean intact(int expected) {
            if (id != expected || pad == null || pad.length != 6) {
                return false;
            }
            for (int i = 0; i < pad.length; i++) {
                if (pad[i] != expected * 31L + i) {
                    return false;
                }
            }
            return true;
        }
    }

    static final int N = Integer.getInteger("sbr3.n", 4000);
    static final int SPIN_EVERY = Integer.getInteger("sbr3.spinEvery", 32);
    static final long SPIN_NS = Long.getLong("sbr3.spinNs", 40_000L);
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        boolean soft = args.length > 1 && args[1].equals("soft");
        long maxMem = Runtime.getRuntime().maxMemory();
        // Ballast: keeps the old generation well above the initiating occupancy.
        List<byte[]> ballast = new ArrayList<>();
        long want = maxMem * Integer.getInteger("sbr3.ballastPct", 35) / 100;
        for (long got = 0; got < want; got += 64 * 1024) {
            ballast.add(new byte[64 * 1024]);
        }
        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong churned = new AtomicLong();
        Thread churn = new Thread(() -> {
            List<byte[]> junk = new ArrayList<>();
            while (!stop.get()) {
                junk.add(new byte[4096]);
                churned.incrementAndGet();
                if (junk.size() > 3000) {
                    junk.clear();
                }
            }
        }, "churn");
        churn.setDaemon(true);
        churn.start();

        Object[] store = new Object[N];
        long deadline = System.nanoTime() + seconds * 1_000_000_000L;
        long rounds = 0;
        long handed = 0;
        long bad = 0;
        long lost = 0;
        long gone = 0;
        java.util.Random rnd = new java.util.Random(42);
        while (System.nanoTime() < deadline) {
            rounds++;
            @SuppressWarnings("unchecked")
            Reference<Payload>[] refs = new Reference[N];
            Object[] hold = new Object[N];
            for (int i = 0; i < N; i++) {
                Payload p = new Payload(i);
                hold[i] = p;
                refs[i] = soft ? new SoftReference<>(p) : new WeakReference<>(p);
            }
            // promote: the payloads must be old before they become weakly reachable
            long target = churned.get() + 60_000;
            while (churned.get() < target) {
                Thread.onSpinWait();
            }
            java.util.Arrays.fill(store, null);
            hold = null;
            // a mark cycle is probably running; take a random slice of it
            long delay = rnd.nextInt(40);
            long until = System.nanoTime() + delay * 1_000_000L;
            while (System.nanoTime() < until) {
                Thread.onSpinWait();
            }
            // spread the hand-out over about one collection period, so a share of it falls inside a
            // mark cycle however the cycles are phased against the round
            for (int i = 0; i < N; i++) {
                Payload p = refs[i].get();
                if (p != null) {
                    store[i] = p;
                    handed++;
                }
                if (SPIN_EVERY > 0 && (i % SPIN_EVERY) == 0) {
                    long t = System.nanoTime() + SPIN_NS;
                    while (System.nanoTime() < t) {
                        Thread.onSpinWait();
                    }
                }
            }
            // wait the cycle out (the churn keeps collecting), then check
            target = churned.get() + 90_000;
            while (churned.get() < target) {
                Thread.onSpinWait();
            }
            for (int i = 0; i < N; i++) {
                Object o = store[i];
                if (o == null) {
                    gone++;
                    continue;
                }
                if (!(o instanceof Payload) || !((Payload) o).intact(i)) {
                    bad++;
                } else if (refs[i].get() != o) {
                    lost++;
                }
            }
            sink = store;
        }
        stop.set(true);
        System.out.println("rounds=" + rounds + " handed=" + handed + " alreadyGone=" + gone + " bad=" + bad
                + " lost=" + lost + " ballast=" + ballast.size());
        System.out.println(bad == 0 && lost == 0 ? "SATB-OK" : "SATB-FAIL");
    }
}
