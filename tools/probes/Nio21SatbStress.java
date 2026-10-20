// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

/**
 * nio21 wave 4, lane L4: the GC stress for the SATB pre-barrier of COMPILED reference overwrites
 * that go through a VarHandle or an Unsafe-backed updater (page
 * {@code r21-varhandle-thin-write-skips-the-generational-native-store-satb-FIXED-20261008.md}).
 *
 * <p>The classic SATB loss, once per element per round: payload {@code P(i, r-1)} is reachable
 * ONLY through {@code holders[i].field} at the mark snapshot (it was promoted by churn first); the
 * hot loop reads it, overwrites the field with {@code P(i, r)} through the mode's store, and
 * stores the old one into {@code store[i]}, an array allocated before the cycle (so usually
 * already scanned). The store into {@code store[i]} logs the OLD element (null), so the payload
 * survives the cycle only if the overwrite's pre-barrier logged it. A store that skips the
 * barrier lets the sweep free a live payload, and the verify pass then sees a payload whose
 * {@code id} / {@code round} / {@code pad} words are not its own (or crashes).
 *
 * <p>Every mode is verified after every round: every {@code store[i]} is the intact
 * {@code P(i, r-1)} and every holder field is the intact {@code P(i, r)}. Output is one
 * deterministic line per mode, {@code <mode> rounds=R n=N bad=0}, then {@code SATB-OK}; HotSpot
 * prints the same. Round count is fixed ({@code -Dnio21.rounds}, default 40), not timed.
 *
 * <pre>
 *   cratonvm --jdk-only -Xmx128m -XX:+UseGenerationalGC -Dnio21.rounds=60 -cp tools/probes Nio21SatbStress
 *   cratonvm --jdk-only -Xmx128m -XX:+UseG1GC -XX:InitiatingHeapOccupancyPercent=10 -cp tools/probes Nio21SatbStress
 *   java -Xmx128m -cp tools/probes Nio21SatbStress           (HotSpot reference output)
 * </pre>
 *
 * Negative control (probabilistic): {@code CRATONVM_NATIVE_STORE_SATB=0} under
 * {@code -XX:+UseGenerationalGC} removes the native-store pre-barrier the thin path now shares.
 */
public final class Nio21SatbStress {
    static final class Payload {
        final int id;
        final int round;
        final long[] pad;

        Payload(int id, int round) {
            this.id = id;
            this.round = round;
            this.pad = new long[4];
            for (int k = 0; k < pad.length; k++) {
                pad[k] = word(id, round, k);
            }
        }

        static long word(int id, int round, int k) {
            return (id * 0x9E3779B97F4A7C15L) ^ ((long) round << 32) ^ (k * 0x5851F42D4C957F2DL);
        }

        boolean intact(int expectId, int expectRound) {
            if (id != expectId || round != expectRound || pad == null || pad.length != 4) {
                return false;
            }
            for (int k = 0; k < 4; k++) {
                if (pad[k] != word(expectId, expectRound, k)) {
                    return false;
                }
            }
            return true;
        }
    }

    static final class Holder {
        volatile Payload v;
        Payload p;
    }

    static final class Node {
        Node next;
        long pad;
    }

    static final VarHandle VH_V;
    static final VarHandle VH_P;
    static final VarHandle VH_ARR = MethodHandles.arrayElementVarHandle(Payload[].class);
    static final AtomicReferenceFieldUpdater<Holder, Payload> ARFU =
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Payload.class, "v");

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            VH_V = l.findVarHandle(Holder.class, "v", Payload.class);
            VH_P = l.findVarHandle(Holder.class, "p", Payload.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static final String[] MODES = {
        "vh-set", "vh-setVolatile", "vh-setRelease", "vh-setOpaque", "vh-cas", "vh-getAndSet",
        "arfu-set", "arfu-lazySet", "arfu-cas", "arfu-getAndSet",
        "arr-setVolatile", "arr-cas", "arr-getAndSet", "putfield",
    };

    static final int N = Integer.getInteger("nio21.n", 4000);
    static final int ROUNDS = Integer.getInteger("nio21.rounds", 40);
    static final int SPIN_EVERY = Integer.getInteger("nio21.spinEvery", 64);
    static final long SPIN_NS = Long.getLong("nio21.spinNs", 20_000L);
    static final int CHAIN = Integer.getInteger("nio21.chain", 200_000);
    static Node chainHead;
    static volatile Object sink;

    /** The field / element the mode stores to, as a plain read. */
    static Payload current(int mode, Holder[] hs, Payload[] arr, int i) {
        if (mode >= 10 && mode <= 12) {
            return arr[i];
        }
        return (mode == 0 || mode == 13) ? hs[i].p : hs[i].v;
    }

    /** The hot loop: read the old payload, overwrite it through the mode's store, keep the old. */
    static void mutate(int mode, Holder[] hs, Payload[] arr, Payload[] store, int round) {
        for (int i = 0; i < hs.length; i++) {
            Holder h = hs[i];
            Payload fresh = new Payload(i, round);
            Payload old;
            switch (mode) {
                case 0: old = h.p; VH_P.set(h, fresh); break;
                case 1: old = h.v; VH_V.setVolatile(h, fresh); break;
                case 2: old = h.v; VH_V.setRelease(h, fresh); break;
                case 3: old = h.v; VH_V.setOpaque(h, fresh); break;
                case 4: old = h.v; if (!VH_V.compareAndSet(h, old, fresh)) old = null; break;
                case 5: old = (Payload) VH_V.getAndSet(h, fresh); break;
                case 6: old = h.v; ARFU.set(h, fresh); break;
                case 7: old = h.v; ARFU.lazySet(h, fresh); break;
                case 8: old = h.v; if (!ARFU.compareAndSet(h, old, fresh)) old = null; break;
                case 9: old = ARFU.getAndSet(h, fresh); break;
                case 10: old = arr[i]; VH_ARR.setVolatile(arr, i, fresh); break;
                case 11: old = arr[i]; if (!VH_ARR.compareAndSet(arr, i, old, fresh)) old = null; break;
                case 12: old = (Payload) VH_ARR.getAndSet(arr, i, fresh); break;
                default: old = h.p; h.p = fresh; break;
            }
            store[i] = old;
            if (SPIN_EVERY > 0 && (i % SPIN_EVERY) == 0) {
                long t = System.nanoTime() + SPIN_NS;
                while (System.nanoTime() < t) {
                    Thread.onSpinWait();
                }
            }
        }
    }

    static void waitChurn(AtomicLong churned, long n) {
        long target = churned.get() + n;
        while (churned.get() < target) {
            Thread.onSpinWait();
        }
    }

    public static void main(String[] args) throws Exception {
        Node head = null;
        for (int i = 0; i < CHAIN; i++) {
            Node n = new Node();
            n.next = head;
            n.pad = i;
            head = n;
        }
        chainHead = head;
        List<byte[]> ballast = new ArrayList<>();
        long want = Runtime.getRuntime().maxMemory() * Integer.getInteger("nio21.ballastPct", 30) / 100;
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

        long totalBad = 0;
        for (int mode = 0; mode < MODES.length; mode++) {
            Holder[] hs = new Holder[N];
            Payload[] arr = new Payload[N];
            Payload[] store = new Payload[N];
            for (int i = 0; i < N; i++) {
                hs[i] = new Holder();
                Payload p = new Payload(i, 0);
                hs[i].v = p;
                hs[i].p = p;
                arr[i] = p;
            }
            long bad = 0;
            for (int round = 1; round <= ROUNDS; round++) {
                // Promote the round's payloads, so the old generation's marker owns them.
                waitChurn(churned, 40_000);
                java.util.Arrays.fill(store, null);
                mutate(mode, hs, arr, store, round);
                // Let a cycle that started during the loop reach its sweep.
                waitChurn(churned, 60_000);
                for (int i = 0; i < N; i++) {
                    Payload old = store[i];
                    if (old == null || !old.intact(i, round - 1)) {
                        bad++;
                    }
                    Payload now = current(mode, hs, arr, i);
                    if (now == null || !now.intact(i, round)) {
                        bad++;
                    }
                }
                // The non-written fields still hold round 0's payload; keep them honest too.
                if (mode != 0 && mode != 13) {
                    for (int i = 0; i < N; i++) {
                        if (hs[i].p == null || !hs[i].p.intact(i, 0)) {
                            bad++;
                        }
                    }
                }
                sink = store;
            }
            System.out.println(MODES[mode] + " rounds=" + ROUNDS + " n=" + N + " bad=" + bad);
            totalBad += bad;
        }
        stop.set(true);
        long chainLen = 0;
        for (Node n = chainHead; n != null; n = n.next) {
            chainLen++;
        }
        System.out.println("chain=" + (chainLen == CHAIN) + " ballast=" + (ballast.size() > 0));
        System.out.println(totalBad == 0 ? "SATB-OK" : "SATB-FAIL");
    }
}
