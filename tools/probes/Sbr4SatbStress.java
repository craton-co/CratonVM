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
 * SBR round 4, lane {@code satb}: the POSITIVE CONTROL for the SATB keep-alive of a BYTECODE
 * {@code Reference.get()}, in every execution tier. Derived from {@code Sbr3RefmiscSatb} (which
 * proved the hazard against the native {@code get}); this one is built so the hand-out goes
 * through a chosen TIER and a chosen static receiver type:
 *
 * <ul>
 *   <li>{@code -Dsbr4.warm=N}: call {@link #read} N times on a live reference first, so the
 *       hand-out loop runs compiled (single-pass / IR) code, not the interpreter;
 *   <li>{@code -Dsbr4.typed=ref|weak}: the static type of the receiver ({@code Reference} or
 *       {@code WeakReference}); {@code get} is the same method either way, the call sites differ.
 * </ul>
 *
 * Each round: N payloads behind one reference each, strongly held, promoted by churn, dropped so
 * they are weakly reachable only, a random delay (a mark cycle is probably under way), then every
 * {@code get()} is stored into {@code store} (allocated before the cycle, so usually already
 * scanned). Failure is a stored element that is not an intact payload of its own index, or a
 * reference cleared while {@code store} still holds the referent.
 *
 * <pre>
 *   cratonvm --jdk-only -Xmx96m -XX:+UseG1GC -XX:InitiatingHeapOccupancyPercent=10 \
 *       -Dsbr3.n=20000 -cp . Sbr4SatbStress [seconds] [weak|soft]
 * </pre>
 */
public final class Sbr4SatbStress {
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
    static final int WARM = Integer.getInteger("sbr4.warm", 0);
    static final int DELAY_MAX = Integer.getInteger("sbr4.delayMax", 40);
    static final boolean TYPED_WEAK = "weak".equals(System.getProperty("sbr4.typed", "ref"));
    static volatile Object sink;
    /** A long old chain (`-Dsbr4.chain=N` nodes) the marker must walk every cycle, so marking spans many
     *  milliseconds and a hand-out spread over the round reliably lands inside a concurrent cycle. */
    static final class Node {
        Node next;
        long pad;
    }

    static final int CHAIN = Integer.getInteger("sbr4.chain", 0);
    static Node chainHead;

    static Object readRef(Reference<?> r) {
        return r.get();
    }

    static Object readWeak(WeakReference<?> r) {
        return r.get();
    }

    static Object read(Reference<?> r, boolean weakTyped) {
        return weakTyped ? readWeak((WeakReference<?>) r) : readRef(r);
    }

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        boolean soft = args.length > 1 && args[1].equals("soft");
        boolean weakTyped = TYPED_WEAK && !soft;
        if (WARM > 0) {
            Payload live = new Payload(7);
            Reference<Payload> wr = soft ? new SoftReference<>(live) : new WeakReference<>(live);
            long acc = 0;
            for (int i = 0; i < WARM; i++) {
                if (read(wr, weakTyped) == live) {
                    acc++;
                }
            }
            if (acc != WARM || live.id != 7) {
                throw new AssertionError("warm-up read answered wrong");
            }
        }
        if (CHAIN > 0) {
            Node h = null;
            for (int i = 0; i < CHAIN; i++) {
                Node n = new Node();
                n.next = h;
                h = n;
            }
            chainHead = h;
        }
        long maxMem = Runtime.getRuntime().maxMemory();
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
            long target = churned.get() + 60_000;
            while (churned.get() < target) {
                Thread.onSpinWait();
            }
            java.util.Arrays.fill(store, null);
            hold = null;
            long delay = rnd.nextInt(DELAY_MAX);
            long until = System.nanoTime() + delay * 1_000_000L;
            while (System.nanoTime() < until) {
                Thread.onSpinWait();
            }
            for (int i = 0; i < N; i++) {
                Object p = read(refs[i], weakTyped);
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
