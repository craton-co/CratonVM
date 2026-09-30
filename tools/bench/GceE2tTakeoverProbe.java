// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * gce e2/t (2026-09-29): a take-over workload whose peers are in compiled
 * code at every pause, for
 * {@code docs/internal/gc-common-round-20260923/common-c-linux-takeover-signals-every-thread-FIXED-20260929.md}
 * and {@code docs/internal/gc/gce-e1t-linux-parked-takeover-peers-spin-a-core-each-FIXED-20260929.md}.
 *
 * <p>{@code WORKERS} threads spin in an allocation-free, call-free compiled
 * loop: a volatile stop flag, sixteen array reads and a read of a holder object
 * per iteration. So the loop's only poll is its back edge. They hold
 * references across the whole loop: their holder and their array, in
 * registers or frame slots. The main thread meanwhile runs {@code ROUNDS}
 * rounds of 8 MB of garbage plus {@code System.gc()}, so every round has at
 * least one pause while every worker is in compiled code. At the end each
 * worker checks that its holder and array are intact.
 *
 * <p><b>Forcing the take-over.</b> Such a loop normally still arrives at the
 * barrier cooperatively: its poll sees the request within nanoseconds, before
 * the take-over's first signal. With {@code CRATONVM_DBG_XT_FORCE_TAKEOVER=1}
 * a compiled poll declines to park, for up to 200 ms per pause, so the
 * take-over freezes the workers in compiled code. On a correct VM every run
 * then reports {@code taken_over>=1} on the {@code [GC] xt_peer_scan} exit
 * line ({@code CRATONVM_GC_STATS=1}).
 *
 * <p>stdout is deterministic and equal to HotSpot's
 * ({@code java -XX:+UseSerialGC -Xmx256m -cp tools/bench GceE2tTakeoverProbe}):
 * <pre>
 *   xt-takeover workers=16 rounds=40 bad=0
 *   PASS
 * </pre>
 *
 * <p>stderr has one line for the measured phase (the GC rounds, while every
 * worker spins):
 * <pre>
 *   [probe] wall_ms=W cpu_ms=C gc_count=N gc_ms=G
 * </pre>
 * {@code cpu_ms} is the process's user+system time from
 * {@code /proc/self/stat} over that phase ({@code na} off Linux), and
 * {@code gc_count} / {@code gc_ms} are the collector MXBeans' deltas. The
 * default of 16 workers oversubscribes an 8-core host on purpose: that is where
 * a frozen peer that spins (rather than yields its core) competes with the
 * collector and with the next peer the take-over must signal. Compare
 * {@code gc_ms} and {@code wall_ms} between arms, not {@code cpu_ms} alone: with
 * free cores a yielding peer burns about as much CPU as a spinning one.
 *
 * <p>Usage: {@code GceE2tTakeoverProbe [workers] [rounds]}.
 */
public final class GceE2tTakeoverProbe {
    static final int LEN = 4096;

    static final class Holder {
        final int id;
        final int magic;
        final int[] data;

        Holder(int id) {
            this.id = id;
            this.magic = id * 0x9E3779B1 ^ 0x5A5A5A5A;
            this.data = new int[LEN];
            for (int i = 0; i < LEN; i++) {
                data[i] = i * 31 + id;
            }
        }

        boolean intact() {
            if (magic != (id * 0x9E3779B1 ^ 0x5A5A5A5A) || data == null || data.length != LEN) {
                return false;
            }
            for (int i = 0; i < LEN; i++) {
                if (data[i] != i * 31 + id) {
                    return false;
                }
            }
            return true;
        }
    }

    static volatile boolean stop;
    static volatile long sink;

    /**
     * The compiled loop: no allocation, no call, one back-edge poll. Reads
     * the holder's fields every iteration so the holder stays live in a
     * register or a frame slot for the whole loop.
     */
    static long spin(Holder h) {
        final int[] d = h.data;
        long acc = 0;
        int i = 0;
        while (!stop) {
            final int b = i & (LEN - 16);
            acc += d[b] + d[b + 1] + d[b + 2] + d[b + 3]
                    + d[b + 4] + d[b + 5] + d[b + 6] + d[b + 7]
                    + d[b + 8] + d[b + 9] + d[b + 10] + d[b + 11]
                    + d[b + 12] + d[b + 13] + d[b + 14] + d[b + 15]
                    + h.magic;
            i += 16;
        }
        return acc;
    }

    static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    /** User+system CPU of this process in ms, from /proc/self/stat; -1 if unavailable. */
    static long cpuMs() {
        try {
            final String s = Files.readString(Path.of("/proc/self/stat"));
            final String[] f = s.substring(s.lastIndexOf(')') + 2).split(" ");
            // f[0] is field 3 (state); utime is field 14, stime field 15.
            final long ticks = Long.parseLong(f[11]) + Long.parseLong(f[12]);
            return ticks * 10; // USER_HZ = 100 on Linux x86-64
        } catch (Throwable t) {
            return -1;
        }
    }

    static long[] gcTotals() {
        long count = 0;
        long ms = 0;
        try {
            for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
                count += Math.max(0, b.getCollectionCount());
                ms += Math.max(0, b.getCollectionTime());
            }
        } catch (Throwable t) {
            return new long[] {-1, -1};
        }
        return new long[] {count, ms};
    }

    public static void main(String[] args) throws Exception {
        final int workers = arg(args, 0, 16);
        final int rounds = arg(args, 1, 40);
        final Holder[] holders = new Holder[workers];
        for (int w = 0; w < workers; w++) {
            holders[w] = new Holder(w + 1);
        }
        // Warm `spin` up to compiled code before the workers start: a short
        // stop/start cycle per warm-up call (the flag flips from another
        // thread after 20 ms).
        for (int k = 0; k < 20; k++) {
            stop = false;
            final Thread flip = new Thread(() -> {
                try {
                    Thread.sleep(20);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
                stop = true;
            });
            flip.start();
            sink += spin(holders[0]);
            flip.join();
        }
        stop = false;
        // Each worker TAKES its holder out of `inbox` and puts it in `outbox`
        // only after its loop ends, so while it spins the holder is reachable
        // from that worker's compiled frame alone: a frozen worker whose
        // registers and frame the take-over failed to scan loses it.
        final java.util.concurrent.atomic.AtomicReferenceArray<Holder> inbox =
                new java.util.concurrent.atomic.AtomicReferenceArray<>(holders);
        final java.util.concurrent.atomic.AtomicReferenceArray<Holder> outbox =
                new java.util.concurrent.atomic.AtomicReferenceArray<>(workers);
        java.util.Arrays.fill(holders, null);
        final Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            final int me = w;
            ts[w] = new Thread(() -> {
                final Holder h = inbox.getAndSet(me, null);
                sink += spin(h);
                outbox.set(me, h);
            }, "spin-" + w);
            ts[w].start();
        }
        for (int w = 0; w < workers; w++) {
            while (inbox.get(w) != null) {
                Thread.sleep(1);
            }
        }
        Thread.sleep(200); // every worker is in its compiled loop
        final long cpu0 = cpuMs();
        final long[] gc0 = gcTotals();
        final long t0 = System.nanoTime();
        long garbage = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 8 * 1024; i++) {
                final byte[] g = new byte[1024];
                g[i & 1023] = (byte) i;
                garbage += g[i & 1023];
            }
            System.gc();
        }
        final long wallMs = (System.nanoTime() - t0) / 1_000_000;
        final long cpu1 = cpuMs();
        final long[] gc1 = gcTotals();
        stop = true;
        for (Thread t : ts) {
            t.join();
        }
        sink += garbage;
        int bad = 0;
        for (int w = 0; w < workers; w++) {
            final Holder h = outbox.get(w);
            if (h == null || h.id != w + 1 || !h.intact()) {
                bad++;
            }
        }
        System.err.println("[probe] wall_ms=" + wallMs
                + " cpu_ms=" + (cpu0 < 0 || cpu1 < 0 ? "na" : Long.toString(cpu1 - cpu0))
                + " gc_count=" + (gc0[0] < 0 ? "na" : Long.toString(gc1[0] - gc0[0]))
                + " gc_ms=" + (gc0[1] < 0 ? "na" : Long.toString(gc1[1] - gc0[1])));
        System.out.println("xt-takeover workers=" + workers + " rounds=" + rounds + " bad=" + bad);
        System.out.println(bad == 0 ? "PASS" : "FAIL");
        if (bad != 0) {
            System.exit(1);
        }
    }
}
