// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

/**
 * gen r5w5/sizer9 (2026-09-27): the allocation shape that exercises the TLAB
 * share sizer's refill-waste KEEP ({@code CRATONVM_TLAB_SHARE_SIZER=1}) together
 * with the Generational tail sink, and checks that no live byte is handed out
 * twice. See
 * {@code docs/internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md}.
 *
 * <p>Each of {@code threads} workers allocates {@code rounds} x 512 {@code long[]}s
 * whose sizes cycle from 1 KiB to 31 KiB, so a TLAB miss often leaves a tail
 * larger than the refill-waste limit (the keep arm), keeps the last 256 of
 * them alive in a ring and checks each one's first and last element when it is
 * evicted, replaces a small {@code Cell} garbage object per array, and every
 * 64 arrays checks and replaces {@code holder} — a {@code static volatile} that
 * is only ever assigned a live, self-checking {@code Cell} (the
 * {@code GenR5W3ConcUnloadProbe} shape that read back null). Every eighth
 * round it parks for 20 us, which retires the TLAB early (the tail sink's
 * input). A zeroed or reused live object shows up as {@code corrupt=} or
 * {@code null_holder=} above zero.
 *
 * <p>Deterministic output ({@code sum=} depends only on the arguments). HotSpot
 * ({@code java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W5KeepChurnProbe})
 * prints, for the defaults:
 * <pre>
 *   keep-churn threads=4 rounds=200 ring=256 corrupt=0 null_holder=0 sum=20866589184
 *   PASS
 * </pre>
 * Commands (JIT on, so the keep arm is reachable; the interpreter never keeps):
 * <pre>
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m --verbose:gc -cp tools/bench GenR5W5KeepChurnProbe
 *   CRATONVM_TLAB_SHARE_SIZER=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m --verbose:gc -cp tools/bench GenR5W5KeepChurnProbe
 * </pre>
 * With the sizer on, {@code [GC] tlab-sizer: sizer_keeps=} must be above zero
 * (else the run did not reach the keep arm) and
 * {@code [GC] tlab-guard: filler_over_object=0 refill_over_object=0}.
 */
public final class GenR5W5KeepChurnProbe {
    static final int RING = 256;
    static final int PER_ROUND = 512;

    static final class Cell {
        final int id;
        final long check;
        final long[] body = new long[4];

        Cell(int id) {
            this.id = id;
            this.check = mix(id);
            for (int i = 0; i < body.length; i++) {
                body[i] = check ^ i;
            }
        }

        static long mix(int id) {
            long x = id * 0x9E3779B97F4A7C15L;
            return x ^ (x >>> 29);
        }

        boolean ok() {
            if (check != mix(id) || body.length != 4) {
                return false;
            }
            for (int i = 0; i < body.length; i++) {
                if (body[i] != (check ^ i)) {
                    return false;
                }
            }
            return true;
        }
    }

    /** Only ever assigned a live, self-checking {@code Cell}. */
    static volatile Object holder;
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        final int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        final long[] sums = new long[threads];
        final long[] corrupt = new long[threads];
        final long[] nullHolder = new long[threads];
        holder = new Cell(0);
        final CountDownLatch done = new CountDownLatch(threads);
        for (int t = 0; t < threads; t++) {
            final int id = t;
            Thread th = new Thread(() -> {
                long[][] live = new long[RING][];
                long s = 0;
                long bad = 0;
                long nulls = 0;
                for (int r = 0; r < rounds; r++) {
                    for (int k = 0; k < PER_ROUND; k++) {
                        int idx = r * PER_ROUND + k;
                        // 128 .. 3967 longs: 1 KiB .. 31 KiB, below the 32 KiB
                        // TLAB cap, so every one goes through the TLAB path.
                        int len = 128 + (int) ((idx * 7919L) % 3840L);
                        long[] a = new long[len];
                        a[0] = idx;
                        a[len - 1] = ~(long) idx;
                        int slot = idx % RING;
                        long[] old = live[slot];
                        if (old != null) {
                            long want = idx - RING;
                            if (old[0] != want || old[old.length - 1] != ~want) {
                                bad++;
                            }
                            s += old[0];
                        }
                        live[slot] = a;
                        sink = new Cell(idx);
                        if ((idx & 63) == 0) {
                            Object h = holder;
                            if (h == null) {
                                nulls++;
                            } else if (!((Cell) h).ok()) {
                                bad++;
                            }
                            holder = new Cell(idx);
                        }
                    }
                    if ((r & 7) == 0) {
                        LockSupport.parkNanos(20_000L);
                    }
                }
                sums[id] = s;
                corrupt[id] = bad;
                nullHolder[id] = nulls;
                done.countDown();
            }, "keep-churn-" + t);
            th.start();
        }
        done.await();
        long sum = 0;
        long bad = 0;
        long nulls = 0;
        for (int t = 0; t < threads; t++) {
            sum += sums[t];
            bad += corrupt[t];
            nulls += nullHolder[t];
        }
        sink = null;
        System.out.println("keep-churn threads=" + threads + " rounds=" + rounds + " ring=" + RING
                + " corrupt=" + bad + " null_holder=" + nulls + " sum=" + sum);
        boolean ok = bad == 0 && nulls == 0 && holder != null;
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
