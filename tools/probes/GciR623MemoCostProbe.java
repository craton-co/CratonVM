// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;
import java.util.concurrent.locks.LockSupport;

/**
 * gci i23/r6 (2026-10-15): the cost and soundness probe of
 * {@code CRATONVM_JIT_UNREG_MEMO_GENERATION_KEYED} (proposal
 * {@code gengc-r5w2-roots6-proposal-generation-keyed-jit-scan-memo-20260926}).
 *
 * <p>Two worker threads hand a turn back and forth, {@code handoffs} times
 * each, from a compiled loop entered below {@code depth} interpreted frames:
 * phase {@code park} through {@code LockSupport.park} / {@code unpark}, phase
 * {@code wait} through {@code Object.wait} / {@code notifyAll}. Every handoff
 * blocks, and every block deposits the thread's roots, which probes the native
 * stack band above the outermost compiled entry (the {@code depth} interpreted
 * frames) for a JIT return address. With the switch on, a clean verdict over
 * that band is reused while the JIT entry chain is untouched. Run with
 * {@code CRATONVM_JIT_DENY=GciR623MemoCostProbe.descend} so the recursion stays
 * interpreted (the band stays {@code depth} frames wide) on every run.
 *
 * <p>Each iteration allocates a young {@code long[16]} that is read back only
 * after the block (a young object live in a compiled frame across the
 * deposit) and a 512-byte garbage array (young collections during the run).
 *
 * <p>stdout is deterministic and identical on HotSpot: per phase one line
 * {@code PASS r6memo mode=<park|wait> handoffs=<n> depth=<d> checksum=<c>}
 * ({@code FAIL} if any run's checksum differs from the computed one). Timings
 * are on stderr only: {@code [r6-memo] <mode> median=<ns>ns/handoff
 * reps_ms=[...]}.
 *
 * <p>Usage: {@code GciR623MemoCostProbe [handoffs=20000] [depth=48] [reps=5]}.
 */
public final class GciR623MemoCostProbe {

    static volatile int turn;
    static final Object MON = new Object();
    static int monTurn; // guarded by MON
    static volatile Object sink;

    static final class Worker extends Thread {
        final int me;
        final int n;
        final int depth;
        final boolean park;
        Thread other;
        long sum;

        Worker(int me, int n, int depth, boolean park) {
            super("r6memo-" + (park ? "park" : "wait") + "-" + me);
            this.me = me;
            this.n = n;
            this.depth = depth;
            this.park = park;
        }

        @Override
        public void run() {
            sum = descend(depth, this);
        }
    }

    /** Interpreted (CRATONVM_JIT_DENY) recursion: the band above the compiled loop. */
    static long descend(int d, Worker w) {
        if (d <= 0) {
            return w.park ? parkLoop(w) : waitLoop(w);
        }
        long r = descend(d - 1, w);
        return r + (d & 1) - (d & 1);
    }

    static long parkLoop(Worker w) {
        long sum = 0;
        final int me = w.me;
        for (int i = 0; i < w.n; i++) {
            long[] box = new long[16];
            box[i & 15] = (long) i * 7 + me;
            sink = new byte[512];
            while (turn != me) {
                LockSupport.park(MON);
            }
            sum += box[i & 15] + box.length;
            turn = 1 - me;
            LockSupport.unpark(w.other);
        }
        return sum;
    }

    static long waitLoop(Worker w) {
        long sum = 0;
        final int me = w.me;
        for (int i = 0; i < w.n; i++) {
            long[] box = new long[16];
            box[i & 15] = (long) i * 11 + me;
            sink = new byte[512];
            synchronized (MON) {
                while (monTurn != me) {
                    try {
                        MON.wait();
                    } catch (InterruptedException e) {
                        throw new AssertionError(e);
                    }
                }
                monTurn = 1 - me;
                MON.notifyAll();
            }
            sum += box[i & 15] + box.length;
        }
        return sum;
    }

    /** Runs one ping-pong; returns {checksum, elapsed ns}. */
    static long[] runOnce(boolean park, int n, int depth) throws InterruptedException {
        turn = 0;
        synchronized (MON) {
            monTurn = 0;
        }
        Worker a = new Worker(0, n, depth, park);
        Worker b = new Worker(1, n, depth, park);
        a.other = b;
        b.other = a;
        long t0 = System.nanoTime();
        a.start();
        b.start();
        a.join();
        b.join();
        long t1 = System.nanoTime();
        return new long[] {a.sum + 31 * b.sum, t1 - t0};
    }

    static long expected(boolean park, int n) {
        long k = park ? 7 : 11;
        long a = 0;
        long b = 0;
        for (int i = 0; i < n; i++) {
            a += k * i + 0 + 16;
            b += k * i + 1 + 16;
        }
        return a + 31 * b;
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int depth = args.length > 1 ? Integer.parseInt(args[1]) : 48;
        int reps = args.length > 2 ? Integer.parseInt(args[2]) : 5;
        String[] modes = {"park", "wait"};
        for (String mode : modes) {
            boolean park = mode.equals("park");
            int warm = Math.max(1, n / 4);
            boolean ok = runOnce(park, warm, depth)[0] == expected(park, warm);
            long want = expected(park, n);
            long checksum = 0;
            long[] ns = new long[reps];
            long[] ms = new long[reps];
            for (int r = 0; r < reps; r++) {
                long[] res = runOnce(park, n, depth);
                if (res[0] != want) {
                    ok = false;
                }
                checksum = res[0];
                ns[r] = res[1];
                ms[r] = res[1] / 1_000_000L;
            }
            long[] sorted = ns.clone();
            Arrays.sort(sorted);
            long median = sorted[reps / 2] / (2L * n);
            System.err.println("[r6-memo] " + mode + " median=" + median + "ns/handoff reps_ms="
                    + Arrays.toString(ms));
            System.out.println((ok ? "PASS" : "FAIL") + " r6memo mode=" + mode + " handoffs=" + n
                    + " depth=" + depth + " checksum=" + checksum);
        }
    }
}
